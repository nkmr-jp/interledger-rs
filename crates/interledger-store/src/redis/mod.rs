// The informal schema of our data in redis:
//   send_routes_to         set         used for CCP routing
//   receive_routes_from    set         used for CCP routing
//   next_account_id        string      unique ID for each new account
//   rates:current          hash        exchange rates
//   routes:current         hash        dynamic routing table
//   routes:static          hash        static routing table
//   accounts:<id>          hash        information for each account
//   btp_outgoing
// For interactive exploration of the store,
// use the redis-cli tool included with your redis install.
// Within redis-cli:
//    keys *                list all keys of any type in the store
//    smembers <key>        list the members of a set
//    get <key>             get the value of a key
//    hgetall <key>         the flattened list of every key/value entry within a hash
mod reconnect;
use reconnect::RedisReconnect;

use super::account::{Account, AccountWithEncryptedTokens};
use super::crypto::{encrypt_token, generate_keys, DecryptionKey, EncryptionKey};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::channel::mpsc::UnboundedSender;
use http::StatusCode;
use interledger_api::{AccountDetails, AccountSettings, EncryptedAccountSettings, NodeStore};
use interledger_btp::BtpStore;
use interledger_ccp::{CcpRoutingAccount, CcpRoutingStore, RoutingRelation};
use interledger_errors::*;
use interledger_http::HttpStore;
use interledger_packet::Address;
use interledger_rates::ExchangeRateStore;
use interledger_router::RouterStore;
use interledger_service::{Account as AccountTrait, AccountStore, AddressStore, Username};
use interledger_service_util::{
    BalanceStore, RateLimitError, RateLimitStore, DEFAULT_ROUND_TRIP_TIME,
};
use interledger_settlement::core::{
    idempotency::{IdempotentData, IdempotentStore},
    scale_with_precision_loss,
    types::{Convert, ConvertDetails, LeftoversStore, SettlementStore},
};
use interledger_stream::{PaymentNotification, StreamNotificationsStore};
use num_bigint::BigUint;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use redis_crate::AsyncCommands;
use redis_crate::{
    self, cmd, from_redis_value, Client, ConnectionInfo, ControlFlow, ErrorKind, FromRedisValue,
    PubSubCommands, RedisError, RedisWrite, Script, ToRedisArgs, Value,
};
use secrecy::{ExposeSecret, Secret, SecretBytesMut};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};
use std::{
    iter::{self, FromIterator},
    str,
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::sync::broadcast;
use tracing::{debug, error, trace, warn};
use url::Url;
use uuid::Uuid;
use zeroize::Zeroize;

use json_logger::LOGGING;
use slog::{info as sinfo};
use chrono;

const DEFAULT_POLL_INTERVAL: u64 = 30000; // 30 seconds
const ACCOUNT_DETAILS_FIELDS: usize = 21;

static PARENT_ILP_KEY: &str = "parent_node_account_address";
static ROUTES_KEY: &str = "routes:current";
static STATIC_ROUTES_KEY: &str = "routes:static";
static DEFAULT_ROUTE_KEY: &str = "routes:default";
static STREAM_NOTIFICATIONS_PREFIX: &str = "stream_notifications:";
static SETTLEMENT_ENGINES_KEY: &str = "settlement_engines";

/// Domain separator for leftover amounts
fn uncredited_amount_key(account_id: impl ToString) -> String {
    format!("uncredited-amount:{}", account_id.to_string())
}

/// Domain separator for idempotency keys
fn prefixed_idempotency_key(idempotency_key: &str) -> String {
    format!("idempotency-key:{}", idempotency_key)
}

/// Domain separator for accounts
fn accounts_key(account_id: Uuid) -> String {
    format!("accounts:{}", account_id)
}

// TODO: Add descriptive errors inside the lua scripts!

// The following are Lua scripts that are used to atomically execute the given logic
// inside Redis. This allows for more complex logic without needing multiple round
// trips for messages to be sent to and from Redis, as well as locks to ensure no other
// process is accessing Redis at the same time.
// For more information on scripting in Redis, see https://redis.io/commands/eval

/// The node's default ILP Address
static DEFAULT_ILP_ADDRESS: Lazy<Address> = Lazy::new(|| Address::from_str("local.host").unwrap());

/// This lua script fetches an account associated with a username. The client
/// MUST ensure that the returned account is authenticated.
static ACCOUNT_FROM_USERNAME: Lazy<Script> =
    Lazy::new(|| Script::new(include_str!("lua/account_from_username.lua")));

/// Lua script which loads a list of accounts
/// If an account does not have a settlement_engine_url set
/// but there is one configured for that account's currency,
/// it will use the globally configured url
static LOAD_ACCOUNTS: Lazy<Script> =
    Lazy::new(|| Script::new(include_str!("lua/load_accounts.lua")));

/// Lua script which reduces the provided account's balance before sending a Prepare packet
static PROCESS_PREPARE: Lazy<Script> =
    Lazy::new(|| Script::new(include_str!("lua/process_prepare.lua")));

/// Lua script which increases the provided account's balance after receiving a Fulfill packet
static PROCESS_FULFILL: Lazy<Script> =
    Lazy::new(|| Script::new(include_str!("lua/process_fulfill.lua")));

/// Lua script which increases the provided account's balance after receiving a Reject packet
static PROCESS_REJECT: Lazy<Script> =
    Lazy::new(|| Script::new(include_str!("lua/process_reject.lua")));

/// Lua script which increases the provided account's balance after a settlement attempt failed
static REFUND_SETTLEMENT: Lazy<Script> =
    Lazy::new(|| Script::new(include_str!("lua/refund_settlement.lua")));

/// Lua script which increases the provided account's balance after an incoming settlement succeeded
static PROCESS_INCOMING_SETTLEMENT: Lazy<Script> =
    Lazy::new(|| Script::new(include_str!("lua/process_incoming_settlement.lua")));

/// Builder for the Redis Store
pub struct RedisStoreBuilder {
    redis_url: ConnectionInfo,
    secret: [u8; 32],
    poll_interval: u64,
    /// Connector's ILP Address. Used to insert `Child` accounts as
    node_ilp_address: Address,
}

impl RedisStoreBuilder {
    /// Simple Constructor
    pub fn new(redis_url: ConnectionInfo, secret: [u8; 32]) -> Self {
        RedisStoreBuilder {
            redis_url,
            secret,
            poll_interval: DEFAULT_POLL_INTERVAL,
            node_ilp_address: DEFAULT_ILP_ADDRESS.clone(),
        }
    }

    /// Sets the ILP Address corresponding to the node
    pub fn node_ilp_address(&mut self, node_ilp_address: Address) -> &mut Self {
        self.node_ilp_address = node_ilp_address;
        self
    }

    /// Sets the poll interval at which the store will update its routes
    pub fn poll_interval(&mut self, poll_interval: u64) -> &mut Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Connects to the Redis Store
    ///
    /// Specifically
    /// 1. Generates encryption and decryption keys
    /// 1. Connects to the redis store (ensuring that it reconnects in case of drop)
    /// 1. Gets the Node address assigned to us by our parent (if it exists)
    /// 1. Starts polling for routing table updates
    /// 1. Spawns a thread to notify incoming payments over WebSockets
    pub async fn connect(&mut self) -> Result<RedisStore, ()> {
        let redis_info = self.redis_url.clone();
        let (encryption_key, decryption_key) = generate_keys(&self.secret[..]);
        self.secret.zeroize(); // clear the secret after it has been used for key generation
        let poll_interval = self.poll_interval;
        let ilp_address = self.node_ilp_address.clone();

        let client = Client::open(redis_info.clone())
            .map_err(|err| error!("Error creating subscription Redis client: {:?}", err))?;
        debug!("Connected subscription client to redis: {:?}", client);
        let mut connection = RedisReconnect::connect(redis_info.clone())
            .map_err(|_| ())
            .await?;
        let mut sub_connection = client
            .get_connection()
            .map_err(|err| error!("Error connecting subscription client to Redis: {:?}", err))?;
        // Before initializing the store, check if we have an address
        // that was configured due to adding a parent. If no parent was
        // found, use the builder's provided address (local.host) or the
        // one we decided to override it with
        let address: Option<String> = connection
            .get(PARENT_ILP_KEY)
            .map_err(|err| {
                error!(
                    "Error checking whether we have a parent configured: {:?}",
                    err
                )
            })
            .await?;
        let node_ilp_address = if let Some(address) = address {
            Address::from_str(&address).unwrap()
        } else {
            ilp_address
        };

        let (all_payment_publisher, _) = broadcast::channel::<PaymentNotification>(256);

        let store = RedisStore {
            ilp_address: Arc::new(RwLock::new(node_ilp_address)),
            connection,
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            payment_publisher: all_payment_publisher,
            exchange_rates: Arc::new(RwLock::new(HashMap::new())),
            routes: Arc::new(RwLock::new(Arc::new(HashMap::new()))),
            encryption_key: Arc::new(encryption_key),
            decryption_key: Arc::new(decryption_key),
        };

        // Poll for routing table updates
        // Note: if this behavior changes, make sure to update the Drop implementation
        let connection_clone = Arc::downgrade(&store.connection.conn);
        let redis_info = store.connection.redis_info.clone();
        let routing_table = store.routes.clone();

        let poll_routes = async move {
            let mut interval = tokio::time::interval(Duration::from_millis(poll_interval));
            // Irrefutable while pattern, can we do something here?
            loop {
                interval.tick().await;
                if let Some(conn) = connection_clone.upgrade() {
                    let _ = update_routes(
                        RedisReconnect {
                            conn,
                            redis_info: redis_info.clone(),
                        },
                        routing_table.clone(),
                    )
                    .map_err(|err| error!("{}", err))
                    .await;
                } else {
                    debug!("Not polling routes anymore because connection was closed");
                    break;
                }
            }
            Ok::<(), ()>(())
        };
        tokio::spawn(poll_routes);

        // Here we spawn a worker thread to listen for incoming messages on Redis pub/sub,
        // running a callback for each message received.
        // This currently must be a thread rather than a task due to the redis-rs driver
        // not yet supporting asynchronous subscriptions (see https://github.com/mitsuhiko/redis-rs/issues/183).
        let subscriptions_clone = store.subscriptions.clone();
        let payment_publisher = store.payment_publisher.clone();
        std::thread::spawn(move || {
            #[allow(clippy::cognitive_complexity)]
            let sub_status =
                sub_connection.psubscribe::<_, _, Vec<String>>(&["*"], move |msg| {
                    let channel_name = msg.get_channel_name();
                    if channel_name.starts_with(STREAM_NOTIFICATIONS_PREFIX) {
                        if let Ok(account_id) = Uuid::from_str(&channel_name[STREAM_NOTIFICATIONS_PREFIX.len()..]) {
                            let message: PaymentNotification = match serde_json::from_slice(msg.get_payload_bytes()) {
                                Ok(s) => s,
                                Err(e) => {
                                    error!("Failed to get payload from subscription: {}", e);
                                    return ControlFlow::Continue;
                                }
                            };
                            trace!("Subscribed message received for account {}: {:?}", account_id, message);
                            if payment_publisher.receiver_count() > 0 {
                                if let Err(err) = payment_publisher.send(message.clone()) {
                                    error!("Failed to send a node-wide payment notification: {:?}", err);
                                }
                            }
                            match subscriptions_clone.read().get(&account_id) {
                                Some(sender) => {
                                    if let Err(err) = sender.unbounded_send(message) {
                                        error!("Failed to send message: {}", err);
                                    }
                                }
                                None => trace!("Ignoring message for account {} because there were no open subscriptions", account_id),
                            }
                        } else {
                            error!("Invalid Uuid in channel name: {}", channel_name);
                        }
                    } else {
                        warn!("Ignoring unexpected message from Redis subscription for channel: {}", channel_name);
                    }
                    ControlFlow::Continue
                });
            match sub_status {
                Err(e) => warn!("Could not issue psubscribe to Redis: {}", e),
                Ok(_) => debug!("Successfully subscribed to Redis pubsub"),
            }
        });

        Ok(store)
    }
}

/// A Store that uses Redis as its underlying database.
///
/// This store leverages atomic Redis transactions to do operations such as balance updates.
///
/// Currently the RedisStore polls the database for the routing table and rate updates, but
/// future versions of it will use PubSub to subscribe to updates.
#[derive(Clone)]
pub struct RedisStore {
    /// The Store's ILP Address
    ilp_address: Arc<RwLock<Address>>,
    /// A connection which reconnects if dropped by accident
    connection: RedisReconnect,
    /// WebSocket sender which publishes incoming payment updates
    subscriptions: Arc<RwLock<HashMap<Uuid, UnboundedSender<PaymentNotification>>>>,
    /// A subscriber to all payment notifications, exposed via a WebSocket
    payment_publisher: broadcast::Sender<PaymentNotification>,
    exchange_rates: Arc<RwLock<HashMap<String, f64>>>,
    /// The store keeps the routing table in memory so that it can be returned
    /// synchronously while the Router is processing packets.
    /// The outer `Arc<RwLock>` is used so that we can update the stored routing
    /// table after polling the store for updates.
    /// The inner `Arc<HashMap>` is used so that the `routing_table` method can
    /// return a reference to the routing table without cloning the underlying data.
    routes: Arc<RwLock<Arc<HashMap<String, Uuid>>>>,
    /// Encryption Key so that the no cleartext data are stored
    encryption_key: Arc<Secret<EncryptionKey>>,
    /// Decryption Key to provide cleartext data to users
    decryption_key: Arc<Secret<DecryptionKey>>,
}

impl RedisStore {
    /// Gets all the account ids from Redis
    async fn get_all_accounts_ids(&self) -> Result<Vec<Uuid>, NodeStoreError> {
        let mut connection = self.connection.clone();
        let account_ids: Vec<RedisAccountId> = connection.smembers("accounts").await?;
        Ok(account_ids.iter().map(|rid| rid.0).collect())
    }

    /// Inserts the account corresponding to the provided `AccountWithEncryptedtokens`
    /// in Redis. Returns the provided account (tokens remain encrypted)
    async fn redis_insert_account(
        &self,
        encrypted: &AccountWithEncryptedTokens,
    ) -> Result<(), NodeStoreError> {
        let account = &encrypted.account;
        let id = accounts_key(account.id);
        let mut connection = self.connection.clone();
        let routing_table = self.routes.clone();
        // Check that there isn't already an account with values that MUST be unique
        let mut pipe = redis_crate::pipe();
        pipe.exists(accounts_key(account.id));
        pipe.hexists("usernames", account.username().as_ref());
        if account.routing_relation == RoutingRelation::Parent {
            pipe.exists(PARENT_ILP_KEY);
        }

        let results: Vec<bool> = pipe.query_async(&mut connection).await?;
        if results.iter().any(|val| *val) {
            warn!(
                "An account already exists with the same {}. Cannot insert account: {:?}",
                account.id, account
            );
            return Err(NodeStoreError::AccountExists(account.username.to_string()));
        }

        let mut pipe = redis_crate::pipe();
        pipe.atomic();

        // Add the account key to the list of accounts
        pipe.sadd("accounts", RedisAccountId(account.id)).ignore();

        // Save map for Username -> Account ID
        pipe.hset(
            "usernames",
            account.username().as_ref(),
            RedisAccountId(account.id),
        )
        .ignore();

        // Set balance-related details
        pipe.hset_multiple(&id, &[("balance", 0), ("prepaid_amount", 0)])
            .ignore();

        if account.should_send_routes() {
            pipe.sadd("send_routes_to", RedisAccountId(account.id))
                .ignore();
        }

        if account.should_receive_routes() {
            pipe.sadd("receive_routes_from", RedisAccountId(account.id))
                .ignore();
        }

        if account.ilp_over_btp_url.is_some() {
            pipe.sadd("btp_outgoing", RedisAccountId(account.id))
                .ignore();
        }

        // Add route to routing table
        pipe.hset(
            ROUTES_KEY,
            account.ilp_address.as_bytes(),
            RedisAccountId(account.id),
        )
        .ignore();

        // Set account details
        pipe.cmd("HMSET").arg(&id).arg(encrypted).ignore();

        // The parent account settings are done via the API. We just
        // had to check for the existence of a parent
        pipe.query_async(&mut connection).await?;

        update_routes(connection, routing_table).await?;
        debug!(
            "Inserted account {} (ILP address: {})",
            account.id, account.ilp_address
        );
        Ok(())
    }

    /// Overwrites the account corresponding to the provided `AccountWithEncryptedtokens`
    /// in Redis. Returns the provided account (tokens remain encrypted)
    async fn redis_update_account(
        &self,
        encrypted: &AccountWithEncryptedTokens,
    ) -> Result<(), NodeStoreError> {
        let account = encrypted.account.clone();
        let mut connection = self.connection.clone();
        let routing_table = self.routes.clone();

        // Check to make sure an account with this ID already exists
        // TODO this needs to be atomic with the insertions later,
        // waiting on #186
        // TODO: Do not allow this update to happen if
        // AccountDetails.RoutingRelation == Parent and parent is
        // already set
        let exists: bool = connection.exists(accounts_key(account.id)).await?;

        if !exists {
            warn!(
                "No account exists with ID {}, cannot update account {:?}",
                account.id, account
            );
            return Err(NodeStoreError::AccountNotFound(account.id.to_string()));
        }
        let mut pipe = redis_crate::pipe();
        pipe.atomic();

        // Add the account key to the list of accounts
        pipe.sadd("accounts", RedisAccountId(account.id)).ignore();

        // Set account details
        pipe.cmd("HMSET")
            .arg(accounts_key(account.id))
            .arg(encrypted)
            .ignore();

        if account.should_send_routes() {
            pipe.sadd("send_routes_to", RedisAccountId(account.id))
                .ignore();
        }

        if account.should_receive_routes() {
            pipe.sadd("receive_routes_from", RedisAccountId(account.id))
                .ignore();
        }

        if account.ilp_over_btp_url.is_some() {
            pipe.sadd("btp_outgoing", RedisAccountId(account.id))
                .ignore();
        }

        // Add route to routing table
        pipe.hset(
            ROUTES_KEY,
            account.ilp_address.to_bytes().to_vec(),
            RedisAccountId(account.id),
        )
        .ignore();

        pipe.query_async(&mut connection).await?;
        update_routes(connection, routing_table).await?;
        debug!(
            "Inserted account {} (id: {}, ILP address: {})",
            account.username, account.id, account.ilp_address
        );
        Ok(())
    }

    /// Modifies the account corresponding to the provided `id` with the provided `settings`
    /// in Redis. Returns the modified account (tokens remain encrypted)
    async fn redis_modify_account(
        &self,
        id: Uuid,
        settings: EncryptedAccountSettings,
    ) -> Result<AccountWithEncryptedTokens, NodeStoreError> {
        let mut pipe = redis_crate::pipe();
        pipe.atomic();

        if let Some(ref endpoint) = settings.ilp_over_btp_url {
            pipe.hset(accounts_key(id), "ilp_over_btp_url", endpoint);
        }

        if let Some(ref endpoint) = settings.ilp_over_http_url {
            pipe.hset(accounts_key(id), "ilp_over_http_url", endpoint);
        }

        if let Some(ref token) = settings.ilp_over_btp_outgoing_token {
            pipe.hset(
                accounts_key(id),
                "ilp_over_btp_outgoing_token",
                token.as_ref(),
            );
        }

        if let Some(ref token) = settings.ilp_over_http_outgoing_token {
            pipe.hset(
                accounts_key(id),
                "ilp_over_http_outgoing_token",
                token.as_ref(),
            );
        }

        if let Some(ref token) = settings.ilp_over_btp_incoming_token {
            pipe.hset(
                accounts_key(id),
                "ilp_over_btp_incoming_token",
                token.as_ref(),
            );
        }

        if let Some(ref token) = settings.ilp_over_http_incoming_token {
            pipe.hset(
                accounts_key(id),
                "ilp_over_http_incoming_token",
                token.as_ref(),
            );
        }

        if let Some(settle_threshold) = settings.settle_threshold {
            pipe.hset(accounts_key(id), "settle_threshold", settle_threshold);
        }

        if let Some(settle_to) = settings.settle_to {
            if settle_to > std::i64::MAX as u64 {
                // Redis cannot handle values greater than i64::MAX (other stores maybe can though)
                return Err(NodeStoreError::InvalidAccount(
                    CreateAccountError::ParamTooLarge("settle_to".to_owned()),
                ));
            }
            pipe.hset(accounts_key(id), "settle_to", settle_to);
        }

        pipe.query_async(&mut self.connection.clone()).await?;

        // return the updated account
        self.redis_get_account(id).await
    }

    /// Gets the account (tokens remain encrypted) corresponding to the provided `id` from Redis.
    async fn redis_get_account(
        &self,
        id: Uuid,
    ) -> Result<AccountWithEncryptedTokens, NodeStoreError> {
        let mut accounts: Vec<AccountWithEncryptedTokens> = LOAD_ACCOUNTS
            .arg(RedisAccountId(id))
            .invoke_async(&mut self.connection.clone())
            .await?;
        accounts
            .pop()
            .ok_or_else(|| NodeStoreError::AccountNotFound(id.to_string()))
    }

    /// Deletes the account corresponding to the provided `id` from Redis.
    /// Returns the deleted account (tokens remain encrypted)
    async fn redis_delete_account(
        &self,
        id: Uuid,
    ) -> Result<AccountWithEncryptedTokens, NodeStoreError> {
        let encrypted = self.redis_get_account(id).await?;
        let account = &encrypted.account;
        let mut pipe = redis_crate::pipe();
        pipe.atomic();

        pipe.srem("accounts", RedisAccountId(account.id)).ignore();

        pipe.del(accounts_key(account.id)).ignore();
        pipe.hdel("usernames", account.username().as_ref()).ignore();

        if account.should_send_routes() {
            pipe.srem("send_routes_to", RedisAccountId(account.id))
                .ignore();
        }

        if account.should_receive_routes() {
            pipe.srem("receive_routes_from", RedisAccountId(account.id))
                .ignore();
        }

        if account.ilp_over_btp_url.is_some() {
            pipe.srem("btp_outgoing", RedisAccountId(account.id))
                .ignore();
        }

        pipe.hdel(ROUTES_KEY, account.ilp_address.to_bytes().to_vec())
            .ignore();

        pipe.del(uncredited_amount_key(id));

        let mut connection = self.connection.clone();
        pipe.query_async(&mut connection).await?;
        update_routes(connection, self.routes.clone()).await?;
        debug!("Deleted account {}", account.id);
        Ok(encrypted)
    }
}

#[async_trait]
impl AccountStore for RedisStore {
    type Account = Account;

    // TODO cache results to avoid hitting Redis for each packet
    async fn get_accounts(
        &self,
        account_ids: Vec<Uuid>,
    ) -> Result<Vec<Account>, AccountStoreError> {
        let num_accounts = account_ids.len();
        let mut script = LOAD_ACCOUNTS.prepare_invoke();
        for id in account_ids.iter() {
            script.arg(id.to_string());
        }

        // Need to clone the connection here to avoid lifetime errors
        let accounts: Vec<AccountWithEncryptedTokens> =
            script.invoke_async(&mut self.connection.clone()).await?;

        // Decrypt the accounts. TODO: This functionality should be
        // decoupled from redis so that it gets reused by the other backends
        if accounts.len() == num_accounts {
            let accounts = accounts
                .into_iter()
                .map(|account| account.decrypt_tokens(&self.decryption_key.expose_secret().0))
                .collect();
            Ok(accounts)
        } else {
            Err(AccountStoreError::WrongLength {
                expected: num_accounts,
                actual: accounts.len(),
            })
        }
    }

    async fn get_account_id_from_username(
        &self,
        username: &Username,
    ) -> Result<Uuid, AccountStoreError> {
        let username = username.clone();
        let id: Option<RedisAccountId> = self
            .connection
            .clone()
            .hget("usernames", username.as_ref())
            .await?;
        match id {
            Some(rid) => Ok(rid.0),
            None => {
                debug!("Username not found: {}", username);
                Err(AccountStoreError::AccountNotFound(username.to_string()))
            }
        }
    }
}

impl StreamNotificationsStore for RedisStore {
    type Account = Account;

    fn add_payment_notification_subscription(
        &self,
        id: Uuid,
        sender: UnboundedSender<PaymentNotification>,
    ) {
        trace!("Added payment notification listener for {}", id);
        self.subscriptions.write().insert(id, sender);
    }

    fn publish_payment_notification(&self, payment: PaymentNotification) {
        let username = payment.to_username.clone();
        let message = serde_json::to_string(&payment).unwrap();
        let mut connection = self.connection.clone();
        let self_clone = self.clone();
        tokio::spawn(async move {
            let account_id = self_clone
                .get_account_id_from_username(&username)
                .map_err(|_| {
                    error!(
                        "Failed to find account ID corresponding to username: {}",
                        username
                    )
                })
                .await?;

            debug!(
                "Publishing payment notification {} for account {}",
                message, account_id
            );
            // https://github.com/rust-lang/rust/issues/64960#issuecomment-544219926
            let published_args = format!("{}{}", STREAM_NOTIFICATIONS_PREFIX, account_id.clone());
            redis_crate::cmd("PUBLISH")
                .arg(published_args)
                .arg(message)
                .query_async(&mut connection)
                .map_err(move |err| error!("Error publish message to Redis: {:?}", err))
                .await?;

            Ok::<(), ()>(())
        });
    }

    fn all_payment_subscription(&self) -> broadcast::Receiver<PaymentNotification> {
        self.payment_publisher.subscribe()
    }
}

#[async_trait]
impl BalanceStore for RedisStore {
    /// Returns the balance **from the account holder's perspective**, meaning the sum of
    /// the Payable Balance and Pending Outgoing minus the Receivable Balance and the Pending Incoming.
    async fn get_balance(&self, account_id: Uuid) -> Result<i64, BalanceStoreError> {
        let values: Vec<i64> = self
            .connection
            .clone()
            .hget(accounts_key(account_id), &["balance", "prepaid_amount"])
            .await?;

        let balance = values[0];
        let prepaid_amount = values[1];
        Ok(balance + prepaid_amount)
    }

    async fn update_balances_for_prepare(
        &self,
        from_account_id: Uuid,
        incoming_amount: u64,
    ) -> Result<(), BalanceStoreError> {
        // Don't do anything if the amount was 0
        if incoming_amount == 0 {
            return Ok(());
        }

        let balance: i64 = PROCESS_PREPARE
            .arg(RedisAccountId(from_account_id))
            .arg(incoming_amount)
            .invoke_async(&mut self.connection.clone())
            .await?;

        let trace_id = chrono::Local::now().timestamp_nanos(); // debug param
        sinfo!(&LOGGING.logger, "UPDATE_BALANCES_FOR_PREPARE";
            "trace_id" => format!("{:?}", trace_id),
            "function" => "BalanceStore.update_balances_for_prepare()",
            "UpdateArg_incoming_amount"=> format!("{:?}", incoming_amount),
            "UpdateArg_from_account_id"=> format!("{:?}", from_account_id),
            "Result_balance"=> format!("{:?}", balance),
        );

        trace!(
            "Processed prepare with incoming amount: {}. Account {} has balance (including prepaid amount): {} ",
            incoming_amount, from_account_id, balance
        );
        Ok(())
    }

    async fn update_balances_for_fulfill(
        &self,
        to_account_id: Uuid,
        outgoing_amount: u64,
    ) -> Result<(i64, u64), BalanceStoreError> {
        let (balance, amount_to_settle): (i64, u64) = PROCESS_FULFILL
            .arg(RedisAccountId(to_account_id))
            .arg(outgoing_amount)
            .invoke_async(&mut self.connection.clone())
            .await?;

        let trace_id = chrono::Local::now().timestamp_nanos(); // debug param
        sinfo!(&LOGGING.logger, "UPDATE_BALANCES_FOR_FULFILL";
            "trace_id" => format!("{:?}", trace_id),
            "function" => "BalanceStore.update_balances_for_fulfill()",
            "UpdateArg_to_account_id"=> format!("{:?}", to_account_id),
            "UpdateArg_outgoing_amount"=> format!("{:?}", outgoing_amount),
            "Result_balance"=> format!("{:?}", balance),
            "Result_amount_to_settle"=> format!("{:?}", amount_to_settle),
        );

        trace!(
            "Processed fulfill for account {} for outgoing amount {}. Fulfill call result: {} {}",
            to_account_id,
            outgoing_amount,
            balance,
            amount_to_settle,
        );
        Ok((balance, amount_to_settle))
    }

    async fn update_balances_for_reject(
        &self,
        from_account_id: Uuid,
        incoming_amount: u64,
    ) -> Result<(), BalanceStoreError> {
        if incoming_amount == 0 {
            return Ok(());
        }

        let balance: i64 = PROCESS_REJECT
            .arg(RedisAccountId(from_account_id))
            .arg(incoming_amount)
            .invoke_async(&mut self.connection.clone())
            .await?;

        let trace_id = chrono::Local::now().timestamp_nanos(); // debug param
        sinfo!(&LOGGING.logger, "UPDATE_BALANCES_FOR_REJECT";
            "trace_id" => format!("{:?}", trace_id),
            "function" => "BalanceStore.update_balances_for_reject()",
            "UpdateArg_from_account_id"=> format!("{:?}", from_account_id),
            "UpdateArg_outgoing_amount"=> format!("{:?}", incoming_amount),
            "Result_balance"=> format!("{:?}", balance),
        );

        trace!(
            "Processed reject for incoming amount: {}. Account {} has balance (including prepaid amount): {}",
            incoming_amount, from_account_id, balance
        );

        Ok(())
    }
}

impl ExchangeRateStore for RedisStore {
    fn get_exchange_rates(&self, asset_codes: &[&str]) -> Result<Vec<f64>, ExchangeRateStoreError> {
        let rates: Vec<f64> = asset_codes
            .iter()
            .filter_map(|code| (*self.exchange_rates.read()).get(*code).cloned())
            .collect();
        if rates.len() == asset_codes.len() {
            Ok(rates)
        } else {
            // todo add error type
            Err(ExchangeRateStoreError::PairNotFound {
                from: asset_codes[0].to_string(),
                to: asset_codes[1].to_string(),
            })
        }
    }

    fn get_all_exchange_rates(&self) -> Result<HashMap<String, f64>, ExchangeRateStoreError> {
        Ok((*self.exchange_rates.read()).clone())
    }

    fn set_exchange_rates(
        &self,
        rates: HashMap<String, f64>,
    ) -> Result<(), ExchangeRateStoreError> {
        // TODO publish rate updates through a pubsub mechanism to support horizontally scaling nodes
        (*self.exchange_rates.write()) = rates;
        Ok(())
    }
}

#[async_trait]
impl BtpStore for RedisStore {
    type Account = Account;

    async fn get_account_from_btp_auth(
        &self,
        username: &Username,
        token: &str,
    ) -> Result<Self::Account, BtpStoreError> {
        // TODO make sure it can't do script injection!
        // TODO cache the result so we don't hit redis for every packet (is that
        // necessary if redis is often used as a cache?)
        let account: Option<AccountWithEncryptedTokens> = ACCOUNT_FROM_USERNAME
            .arg(username.as_ref())
            .invoke_async(&mut self.connection.clone())
            .await?;

        if let Some(account) = account {
            let account = account.decrypt_tokens(&self.decryption_key.expose_secret().0);
            if let Some(ref t) = account.ilp_over_btp_incoming_token {
                let t = t.expose_secret();
                if t.as_ref() == token.as_bytes() {
                    Ok(account)
                } else {
                    debug!(
                        "Found account {} but BTP auth token was wrong",
                        account.username
                    );
                    Err(BtpStoreError::Unauthorized(username.to_string()))
                }
            } else {
                debug!(
                    "Account {} does not have an incoming btp token configured",
                    account.username
                );
                Err(BtpStoreError::Unauthorized(username.to_string()))
            }
        } else {
            warn!("No account found with BTP token");
            Err(BtpStoreError::AccountNotFound(username.to_string()))
        }
    }

    async fn get_btp_outgoing_accounts(&self) -> Result<Vec<Self::Account>, BtpStoreError> {
        let account_ids: Vec<RedisAccountId> =
            self.connection.clone().smembers("btp_outgoing").await?;
        let account_ids: Vec<Uuid> = account_ids.into_iter().map(|id| id.0).collect();

        if account_ids.is_empty() {
            return Ok(Vec::new());
        }

        let accounts = self.get_accounts(account_ids).await?;
        Ok(accounts)
    }
}

#[async_trait]
impl HttpStore for RedisStore {
    type Account = Account;

    /// Checks if the stored token for the provided account id matches the
    /// provided token, and if so, returns the account associated with that token
    async fn get_account_from_http_auth(
        &self,
        username: &Username,
        token: &str,
    ) -> Result<Self::Account, HttpStoreError> {
        // TODO make sure it can't do script injection!
        let account: Option<AccountWithEncryptedTokens> = ACCOUNT_FROM_USERNAME
            .arg(username.as_ref())
            .invoke_async(&mut self.connection.clone())
            .await?;

        if let Some(account) = account {
            let account = account.decrypt_tokens(&self.decryption_key.expose_secret().0);
            if let Some(ref t) = account.ilp_over_http_incoming_token {
                let t = t.expose_secret();
                if t.as_ref() == token.as_bytes() {
                    Ok(account)
                } else {
                    Err(HttpStoreError::Unauthorized(username.to_string()))
                }
            } else {
                Err(HttpStoreError::Unauthorized(username.to_string()))
            }
        } else {
            warn!("No account found with given HTTP auth");
            Err(HttpStoreError::AccountNotFound(username.to_string()))
        }
    }
}

impl RouterStore for RedisStore {
    fn routing_table(&self) -> Arc<HashMap<String, Uuid>> {
        self.routes.read().clone()
    }
}

#[async_trait]
impl NodeStore for RedisStore {
    type Account = Account;

    async fn insert_account(
        &self,
        account: AccountDetails,
    ) -> Result<Self::Account, NodeStoreError> {
        let id = Uuid::new_v4();
        let account = Account::try_from(id, account, self.get_ilp_address())
            .map_err(NodeStoreError::InvalidAccount)?;
        debug!(
            "Generated account id for {}: {}",
            account.username, account.id
        );
        let encrypted = account
            .clone()
            .encrypt_tokens(&self.encryption_key.expose_secret().0);

        self.redis_insert_account(&encrypted).await?;
        Ok(account)
    }

    async fn delete_account(&self, id: Uuid) -> Result<Account, NodeStoreError> {
        let account = self.redis_delete_account(id).await?;
        Ok(account.decrypt_tokens(&self.decryption_key.expose_secret().0))
    }

    async fn update_account(
        &self,
        id: Uuid,
        account: AccountDetails,
    ) -> Result<Self::Account, NodeStoreError> {
        let account = Account::try_from(id, account, self.get_ilp_address())
            .map_err(NodeStoreError::InvalidAccount)?;

        debug!(
            "Generated account id for {}: {}",
            account.username, account.id
        );
        let encrypted = account
            .clone()
            .encrypt_tokens(&self.encryption_key.expose_secret().0);

        self.redis_update_account(&encrypted).await?;
        Ok(account)
    }

    async fn modify_account_settings(
        &self,
        id: Uuid,
        settings: AccountSettings,
    ) -> Result<Self::Account, NodeStoreError> {
        let settings = EncryptedAccountSettings {
            settle_to: settings.settle_to,
            settle_threshold: settings.settle_threshold,
            ilp_over_btp_url: settings.ilp_over_btp_url,
            ilp_over_http_url: settings.ilp_over_http_url,
            ilp_over_btp_incoming_token: settings.ilp_over_btp_incoming_token.map(|token| {
                encrypt_token(
                    &self.encryption_key.expose_secret().0,
                    token.expose_secret().as_bytes(),
                )
                .freeze()
            }),
            ilp_over_http_incoming_token: settings.ilp_over_http_incoming_token.map(|token| {
                encrypt_token(
                    &self.encryption_key.expose_secret().0,
                    token.expose_secret().as_bytes(),
                )
                .freeze()
            }),
            ilp_over_btp_outgoing_token: settings.ilp_over_btp_outgoing_token.map(|token| {
                encrypt_token(
                    &self.encryption_key.expose_secret().0,
                    token.expose_secret().as_bytes(),
                )
                .freeze()
            }),
            ilp_over_http_outgoing_token: settings.ilp_over_http_outgoing_token.map(|token| {
                encrypt_token(
                    &self.encryption_key.expose_secret().0,
                    token.expose_secret().as_bytes(),
                )
                .freeze()
            }),
        };

        let account = self.redis_modify_account(id, settings).await?;
        Ok(account.decrypt_tokens(&self.decryption_key.expose_secret().0))
    }

    // TODO limit the number of results and page through them
    async fn get_all_accounts(&self) -> Result<Vec<Self::Account>, NodeStoreError> {
        let mut connection = self.connection.clone();

        let account_ids = self.get_all_accounts_ids().await?;

        let mut script = LOAD_ACCOUNTS.prepare_invoke();
        for id in account_ids.iter() {
            script.arg(id.to_string());
        }

        let accounts: Vec<AccountWithEncryptedTokens> =
            script.invoke_async(&mut connection).await?;

        // TODO this should be refactored so that it gets reused in multiple backends
        let accounts: Vec<Account> = accounts
            .into_iter()
            .map(|account| account.decrypt_tokens(&self.decryption_key.expose_secret().0))
            .collect();

        Ok(accounts)
    }

    async fn set_static_routes<R>(&self, routes: R) -> Result<(), NodeStoreError>
    where
        R: IntoIterator<Item = (String, Uuid)> + Send + 'async_trait,
    {
        let mut connection = self.connection.clone();
        let routes: Vec<(String, RedisAccountId)> = routes
            .into_iter()
            .map(|(s, id)| (s, RedisAccountId(id)))
            .collect();
        let accounts: HashSet<_> =
            HashSet::from_iter(routes.iter().map(|(_prefix, account_id)| account_id));
        let mut pipe = redis_crate::pipe();
        for account_id in accounts {
            pipe.exists(accounts_key((*account_id).0));
        }

        let routing_table = self.routes.clone();

        let accounts_exist: Vec<bool> = pipe.query_async(&mut connection).await?;

        if !accounts_exist.iter().all(|a| *a) {
            error!("Error setting static routes because not all of the given accounts exist");
            // TODO add proper error variant for "not all accoutns were found"
            return Err(NodeStoreError::MissingAccounts);
        }

        let mut pipe = redis_crate::pipe();
        pipe.atomic()
            .del(STATIC_ROUTES_KEY)
            .ignore()
            .hset_multiple(STATIC_ROUTES_KEY, &routes)
            .ignore();

        pipe.query_async(&mut connection).await?;

        update_routes(connection, routing_table).await?;
        Ok(())
    }

    async fn set_static_route(
        &self,
        prefix: String,
        account_id: Uuid,
    ) -> Result<(), NodeStoreError> {
        let routing_table = self.routes.clone();
        let mut connection = self.connection.clone();

        let exists: bool = connection.exists(accounts_key(account_id)).await?;
        if !exists {
            error!(
                "Cannot set static route for prefix: {} because account {} does not exist",
                prefix, account_id
            );
            return Err(NodeStoreError::AccountNotFound(account_id.to_string()));
        }

        connection
            .hset(STATIC_ROUTES_KEY, prefix, RedisAccountId(account_id))
            .await?;

        update_routes(connection, routing_table).await?;

        Ok(())
    }

    async fn set_default_route(&self, account_id: Uuid) -> Result<(), NodeStoreError> {
        let routing_table = self.routes.clone();
        // TODO replace this with a lua script to do both calls at once
        let mut connection = self.connection.clone();
        let exists: bool = connection.exists(accounts_key(account_id)).await?;
        if !exists {
            error!(
                "Cannot set default route because account {} does not exist",
                account_id
            );
            return Err(NodeStoreError::AccountNotFound(account_id.to_string()));
        }

        connection
            .set(DEFAULT_ROUTE_KEY, RedisAccountId(account_id))
            .await?;
        debug!("Set default route to account id: {}", account_id);
        update_routes(connection, routing_table).await?;
        Ok(())
    }

    async fn set_settlement_engines(
        &self,
        asset_to_url_map: impl IntoIterator<Item = (String, Url)> + Send + 'async_trait,
    ) -> Result<(), NodeStoreError> {
        let mut connection = self.connection.clone();
        let asset_to_url_map: Vec<(String, String)> = asset_to_url_map
            .into_iter()
            .map(|(asset_code, url)| (asset_code, url.to_string()))
            .collect();
        debug!("Setting settlement engines to {:?}", asset_to_url_map);
        connection
            .hset_multiple(SETTLEMENT_ENGINES_KEY, &asset_to_url_map)
            .await?;
        Ok(())
    }

    async fn get_asset_settlement_engine(
        &self,
        asset_code: &str,
    ) -> Result<Option<Url>, NodeStoreError> {
        let url: Option<String> = self
            .connection
            .clone()
            .hget(SETTLEMENT_ENGINES_KEY, asset_code)
            .await?;
        if let Some(url) = url {
            match Url::parse(url.as_str()) {
                Ok(url) => Ok(Some(url)),
                Err(err) => {
                    error!(
                        "Settlement engine URL loaded from Redis was not a valid URL: {:?}",
                        err
                    );
                    Err(NodeStoreError::InvalidEngineUrl(err.to_string()))
                }
            }
        } else {
            Ok(None)
        }
    }
}

#[async_trait]
impl AddressStore for RedisStore {
    // Updates the ILP address of the store & iterates over all children and
    // updates their ILP Address to match the new address.
    async fn set_ilp_address(&self, ilp_address: Address) -> Result<(), AddressStoreError> {
        debug!("Setting ILP address to: {}", ilp_address);
        let routing_table = self.routes.clone();
        let mut connection = self.connection.clone();

        // Set the ILP address we have in memory
        (*self.ilp_address.write()) = ilp_address.clone();

        // Save it to Redis
        connection
            .set(PARENT_ILP_KEY, ilp_address.as_bytes())
            .await?;

        let accounts = self.get_all_accounts().await?;
        // TODO: This can be an expensive operation if this function
        // gets called often. This currently only gets called when
        // inserting a new parent account in the API. It'd be nice
        // if we could generate a child's ILP address on the fly,
        // instead of having to store the username appended to the
        // node's ilp address. Currently this is not possible, as
        // account.ilp_address() cannot access any state that exists
        // on the store.
        let first_segment = ilp_address
            .segments()
            .rev()
            .next()
            .expect("address did not have a first segment, this should be impossible");
        let mut pipe = redis_crate::pipe();
        for account in &accounts {
            // Update the address and routes of all children and non-routing accounts.
            if account.routing_relation() != RoutingRelation::Parent
                && account.routing_relation() != RoutingRelation::Peer
            {
                // remove the old route
                pipe.hdel(ROUTES_KEY, account.ilp_address.as_bytes())
                    .ignore();

                // if the username of the account ends with the
                // node's address, we're already configured so no
                // need to append anything.
                let new_ilp_address = if first_segment == account.username().to_string() {
                    ilp_address.clone()
                } else {
                    ilp_address
                        .with_suffix(account.username().as_bytes())
                        .unwrap()
                };
                pipe.hset(
                    accounts_key(account.id()),
                    "ilp_address",
                    new_ilp_address.as_bytes(),
                )
                .ignore();

                pipe.hset(
                    ROUTES_KEY,
                    new_ilp_address.as_bytes(),
                    RedisAccountId(account.id()),
                )
                .ignore();
            }
        }

        pipe.query_async(&mut connection.clone()).await?;
        update_routes(connection, routing_table).await?;
        Ok(())
    }

    async fn clear_ilp_address(&self) -> Result<(), AddressStoreError> {
        self.connection
            .clone()
            .del(PARENT_ILP_KEY)
            .map_err(|err| AddressStoreError::Other(Box::new(err)))
            .await?;

        // overwrite the ilp address with the default value
        *(self.ilp_address.write()) = DEFAULT_ILP_ADDRESS.clone();
        Ok(())
    }

    fn get_ilp_address(&self) -> Address {
        // read consumes the Arc<RwLock<T>> so we cannot return a reference
        self.ilp_address.read().clone()
    }
}

type RoutingTable<A> = HashMap<String, A>;

#[async_trait]
impl CcpRoutingStore for RedisStore {
    type Account = Account;

    async fn get_accounts_to_send_routes_to(
        &self,
        ignore_accounts: Vec<Uuid>,
    ) -> Result<Vec<Account>, CcpRoutingStoreError> {
        let account_ids: Vec<RedisAccountId> =
            self.connection.clone().smembers("send_routes_to").await?;
        let account_ids: Vec<Uuid> = account_ids
            .into_iter()
            .map(|id| id.0)
            .filter(|id| !ignore_accounts.contains(&id))
            .collect();
        if account_ids.is_empty() {
            return Ok(Vec::new());
        }

        let accounts = self.get_accounts(account_ids).await?;
        Ok(accounts)
    }

    async fn get_accounts_to_receive_routes_from(
        &self,
    ) -> Result<Vec<Account>, CcpRoutingStoreError> {
        let account_ids: Vec<RedisAccountId> = self
            .connection
            .clone()
            .smembers("receive_routes_from")
            .await?;
        let account_ids: Vec<Uuid> = account_ids.into_iter().map(|id| id.0).collect();

        if account_ids.is_empty() {
            return Ok(Vec::new());
        }

        let accounts = self.get_accounts(account_ids).await?;
        Ok(accounts)
    }

    async fn get_local_and_configured_routes(
        &self,
    ) -> Result<(RoutingTable<Account>, RoutingTable<Account>), CcpRoutingStoreError> {
        let static_routes: Vec<(String, RedisAccountId)> =
            self.connection.clone().hgetall(STATIC_ROUTES_KEY).await?;

        let accounts = self.get_all_accounts().await?;

        let local_table = HashMap::from_iter(
            accounts
                .iter()
                .map(|account| (account.ilp_address.to_string(), account.clone())),
        );

        let account_map: HashMap<Uuid, &Account> =
            HashMap::from_iter(accounts.iter().map(|account| (account.id, account)));
        let configured_table: HashMap<String, Account> = HashMap::from_iter(
            static_routes
                .into_iter()
                .filter_map(|(prefix, account_id)| {
                    if let Some(account) = account_map.get(&account_id.0) {
                        Some((prefix, (*account).clone()))
                    } else {
                        warn!(
                            "No account for ID: {}, ignoring configured route for prefix: {}",
                            account_id, prefix
                        );
                        None
                    }
                }),
        );

        Ok((local_table, configured_table))
    }

    async fn set_routes(
        &mut self,
        routes: impl IntoIterator<Item = (String, Account)> + Send + 'async_trait,
    ) -> Result<(), CcpRoutingStoreError> {
        let routes: Vec<(String, RedisAccountId)> = routes
            .into_iter()
            .map(|(prefix, account)| (prefix, RedisAccountId(account.id)))
            .collect();
        let num_routes = routes.len();
        let mut connection = self.connection.clone();

        // Save routes to Redis
        let mut pipe = redis_crate::pipe();
        pipe.atomic()
            .del(ROUTES_KEY)
            .ignore()
            .hset_multiple(ROUTES_KEY, &routes)
            .ignore();

        pipe.query_async(&mut connection).await?;
        trace!("Saved {} routes to Redis", num_routes);

        update_routes(connection, self.routes.clone()).await?;
        Ok(())
    }
}

#[async_trait]
impl RateLimitStore for RedisStore {
    type Account = Account;

    /// Apply rate limits for number of packets per minute and amount of money per minute
    ///
    /// This uses https://github.com/brandur/redis-cell so the redis-cell module MUST be loaded into redis before this is run
    async fn apply_rate_limits(
        &self,
        account: Account,
        prepare_amount: u64,
    ) -> Result<(), RateLimitError> {
        if account.amount_per_minute_limit.is_some() || account.packets_per_minute_limit.is_some() {
            let mut pipe = redis_crate::pipe();
            let packet_limit = account.packets_per_minute_limit.is_some();
            let amount_limit = account.amount_per_minute_limit.is_some();

            if let Some(limit) = account.packets_per_minute_limit {
                let limit = limit - 1;
                let packets_limit = format!("limit:packets:{}", account.id);
                pipe.cmd("CL.THROTTLE")
                    .arg(packets_limit)
                    .arg(limit)
                    .arg(limit)
                    .arg(60)
                    .arg(1);
            }

            if let Some(limit) = account.amount_per_minute_limit {
                let limit = limit - 1;
                let throughput_limit = format!("limit:throughput:{}", account.id);
                pipe.cmd("CL.THROTTLE")
                    .arg(throughput_limit)
                    // TODO allow separate configuration for burst limit
                    .arg(limit)
                    .arg(limit)
                    .arg(60)
                    .arg(prepare_amount);
            }

            let results: Vec<Vec<i64>> = pipe
                .query_async(&mut self.connection.clone())
                .map_err(|err| {
                    error!("Error applying rate limits: {:?}", err);
                    RateLimitError::StoreError
                })
                .await?;

            if packet_limit && amount_limit {
                if results[0][0] == 1 {
                    Err(RateLimitError::PacketLimitExceeded)
                } else if results[1][0] == 1 {
                    Err(RateLimitError::ThroughputLimitExceeded)
                } else {
                    Ok(())
                }
            } else if packet_limit && results[0][0] == 1 {
                Err(RateLimitError::PacketLimitExceeded)
            } else if amount_limit && results[0][0] == 1 {
                Err(RateLimitError::ThroughputLimitExceeded)
            } else {
                Ok(())
            }
        } else {
            Ok(())
        }
    }

    async fn refund_throughput_limit(
        &self,
        account: Account,
        prepare_amount: u64,
    ) -> Result<(), RateLimitError> {
        if let Some(limit) = account.amount_per_minute_limit {
            let limit = limit - 1;
            let throughput_limit = format!("limit:throughput:{}", account.id);
            cmd("CL.THROTTLE")
                .arg(throughput_limit)
                .arg(limit)
                .arg(limit)
                .arg(60)
                // TODO make sure this doesn't overflow
                .arg(0i64 - (prepare_amount as i64))
                .query_async(&mut self.connection.clone())
                .map_err(|_| RateLimitError::StoreError)
                .await?;
        }

        Ok(())
    }
}

#[async_trait]
impl IdempotentStore for RedisStore {
    async fn load_idempotent_data(
        &self,
        idempotency_key: String,
    ) -> Result<Option<IdempotentData>, IdempotentStoreError> {
        let mut connection = self.connection.clone();
        let ret: HashMap<String, String> = connection
            .hgetall(prefixed_idempotency_key(&idempotency_key))
            .await?;

        if let (Some(status_code), Some(data), Some(input_hash_slice)) = (
            ret.get("status_code"),
            ret.get("data"),
            ret.get("input_hash"),
        ) {
            trace!("Loaded idempotency key {:?} - {:?}", idempotency_key, ret);
            let mut input_hash: [u8; 32] = Default::default();
            input_hash.copy_from_slice(input_hash_slice.as_ref());
            Ok(Some(IdempotentData::new(
                StatusCode::from_str(status_code).unwrap(),
                Bytes::from(data.to_owned()),
                input_hash,
            )))
        } else {
            Ok(None)
        }
    }

    async fn save_idempotent_data(
        &self,
        idempotency_key: String,
        input_hash: [u8; 32],
        status_code: StatusCode,
        data: Bytes,
    ) -> Result<(), IdempotentStoreError> {
        let mut pipe = redis_crate::pipe();
        let mut connection = self.connection.clone();
        pipe.atomic()
            .cmd("HMSET") // cannot use hset_multiple since data and status_code have different types
            .arg(&prefixed_idempotency_key(&idempotency_key))
            .arg("status_code")
            .arg(status_code.as_u16())
            .arg("data")
            .arg(data.as_ref())
            .arg("input_hash")
            .arg(&input_hash)
            .ignore()
            .expire(&prefixed_idempotency_key(&idempotency_key), 86400)
            .ignore();
        pipe.query_async(&mut connection).await?;

        trace!(
            "Cached {:?}: {:?}, {:?}",
            idempotency_key,
            status_code,
            data,
        );
        Ok(())
    }
}

#[async_trait]
impl SettlementStore for RedisStore {
    type Account = Account;

    async fn update_balance_for_incoming_settlement(
        &self,
        account_id: Uuid,
        amount: u64,
        idempotency_key: Option<String>,
    ) -> Result<(), SettlementStoreError> {
        let idempotency_key = idempotency_key.unwrap();
        let balance: i64 = PROCESS_INCOMING_SETTLEMENT
            .arg(RedisAccountId(account_id))
            .arg(amount)
            .arg(idempotency_key)
            .invoke_async(&mut self.connection.clone())
            .await?;
        trace!(
            "Processed incoming settlement from account: {} for amount: {}. Balance is now: {}",
            account_id,
            amount,
            balance
        );
        Ok(())
    }

    async fn refund_settlement(
        &self,
        account_id: Uuid,
        settle_amount: u64,
    ) -> Result<(), SettlementStoreError> {
        trace!(
            "Refunding settlement for account: {} of amount: {}",
            account_id,
            settle_amount
        );
        let balance: i64 = REFUND_SETTLEMENT
            .arg(RedisAccountId(account_id))
            .arg(settle_amount)
            .invoke_async(&mut self.connection.clone())
            .await?;

        trace!(
            "Refunded settlement for account: {} of amount: {}. Balance is now: {}",
            account_id,
            settle_amount,
            balance
        );
        Ok(())
    }
}

// TODO: AmountWithScale is re-implemented on Interledger-Settlement. It'd be nice
// if we could deduplicate this by extracting it to a separate crate which would make
// logical sense
#[derive(Debug, Clone)]
struct AmountWithScale {
    num: BigUint,
    scale: u8,
}

impl ToRedisArgs for AmountWithScale {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + RedisWrite,
    {
        let mut rv = Vec::new();
        self.num.to_string().write_redis_args(&mut rv);
        self.scale.to_string().write_redis_args(&mut rv);
        ToRedisArgs::make_arg_vec(&rv, out);
    }
}

impl AmountWithScale {
    fn parse_multi_values(items: &[Value]) -> Option<Self> {
        // We have to iterate over all values because in this case we're making
        // an lrange call. This returns all the tuple elements in 1 array, and
        // it cannot differentiate between 1 AmountWithScale value or multiple
        // ones. This looks like a limitation of redis.rs
        let len = items.len();
        let mut iter = items.iter();

        let mut max_scale = 0;
        let mut amounts = Vec::new();
        // if redis.rs could parse this properly, we could remove this loop,
        // take 2 elements from the items iterator and return. Then we'd perform
        // the summation and scaling in the consumer of the returned vector.
        for _ in (0..len).step_by(2) {
            let num: String = match iter.next().map(FromRedisValue::from_redis_value) {
                Some(Ok(n)) => n,
                _ => return None,
            };
            let num = match BigUint::from_str(&num) {
                Ok(a) => a,
                Err(_) => return None,
            };

            let scale: u8 = match iter.next().map(FromRedisValue::from_redis_value) {
                Some(Ok(c)) => c,
                _ => return None,
            };

            if scale > max_scale {
                max_scale = scale;
            }
            amounts.push((num, scale));
        }

        // We must scale them to the largest scale, and then add them together
        let mut sum = BigUint::from(0u32);
        for amount in &amounts {
            sum += amount
                .0
                .normalize_scale(ConvertDetails {
                    from: amount.1,
                    to: max_scale,
                })
                .unwrap();
        }

        Some(AmountWithScale {
            num: sum,
            scale: max_scale,
        })
    }
}

impl FromRedisValue for AmountWithScale {
    fn from_redis_value(v: &Value) -> Result<Self, RedisError> {
        if let Value::Bulk(ref items) = *v {
            if let Some(result) = Self::parse_multi_values(items) {
                return Ok(result);
            }
        }
        Err(RedisError::from((
            ErrorKind::TypeError,
            "Cannot parse amount with scale",
        )))
    }
}

#[async_trait]
impl LeftoversStore for RedisStore {
    type AccountId = Uuid;
    type AssetType = BigUint;

    async fn get_uncredited_settlement_amount(
        &self,
        account_id: Uuid,
    ) -> Result<(Self::AssetType, u8), LeftoversStoreError> {
        let mut pipe = redis_crate::pipe();
        pipe.atomic();
        // get the amounts and instantly delete them
        pipe.lrange(uncredited_amount_key(account_id.to_string()), 0, -1);
        pipe.del(uncredited_amount_key(account_id.to_string()))
            .ignore();

        let amounts: Vec<AmountWithScale> = pipe.query_async(&mut self.connection.clone()).await?;

        // this call will only return 1 element
        let amount = amounts[0].to_owned();
        Ok((amount.num, amount.scale))
    }

    async fn save_uncredited_settlement_amount(
        &self,
        account_id: Uuid,
        uncredited_settlement_amount: (Self::AssetType, u8),
    ) -> Result<(), LeftoversStoreError> {
        trace!(
            "Saving uncredited_settlement_amount {:?} {:?}",
            account_id,
            uncredited_settlement_amount
        );
        // We store these amounts as lists of strings
        // because we cannot do BigNumber arithmetic in the store
        // When loading the amounts, we convert them to the appropriate data
        // type and sum them up.
        let mut connection = self.connection.clone();
        connection
            .rpush(
                uncredited_amount_key(account_id),
                AmountWithScale {
                    num: uncredited_settlement_amount.0,
                    scale: uncredited_settlement_amount.1,
                },
            )
            .await?;

        Ok(())
    }

    async fn load_uncredited_settlement_amount(
        &self,
        account_id: Uuid,
        local_scale: u8,
    ) -> Result<Self::AssetType, LeftoversStoreError> {
        trace!("Loading uncredited_settlement_amount {:?}", account_id);
        let amount = self.get_uncredited_settlement_amount(account_id).await?;
        // scale the amount from the max scale to the local scale, and then
        // save any potential leftovers to the store
        let (scaled_amount, precision_loss) =
            scale_with_precision_loss(amount.0, local_scale, amount.1);

        if precision_loss > BigUint::from(0u32) {
            self.connection
                .clone()
                .rpush(
                    uncredited_amount_key(account_id),
                    AmountWithScale {
                        num: precision_loss,
                        scale: std::cmp::max(local_scale, amount.1),
                    },
                )
                .await?;
        }

        Ok(scaled_amount)
    }

    async fn clear_uncredited_settlement_amount(
        &self,
        account_id: Uuid,
    ) -> Result<(), LeftoversStoreError> {
        trace!("Clearing uncredited_settlement_amount {:?}", account_id);
        self.connection
            .clone()
            .del(uncredited_amount_key(account_id))
            .await?;
        Ok(())
    }
}

type RouteVec = Vec<(String, RedisAccountId)>;

use futures::future::TryFutureExt;

// TODO replace this with pubsub when async pubsub is added upstream: https://github.com/mitsuhiko/redis-rs/issues/183
async fn update_routes(
    mut connection: RedisReconnect,
    routing_table: Arc<RwLock<Arc<HashMap<String, Uuid>>>>,
) -> Result<(), RedisError> {
    let mut pipe = redis_crate::pipe();
    pipe.hgetall(ROUTES_KEY)
        .hgetall(STATIC_ROUTES_KEY)
        .get(DEFAULT_ROUTE_KEY);
    let (routes, static_routes, default_route): (RouteVec, RouteVec, Option<RedisAccountId>) =
        pipe.query_async(&mut connection).await?;
    trace!(
        "Loaded routes from redis. Static routes: {:?}, default route: {:?}, other routes: {:?}",
        static_routes,
        default_route,
        routes
    );
    // If there is a default route set in the db,
    // set the entry for "" in the routing table to route to that account
    let default_route_iter = iter::once(default_route)
        .filter_map(|r| r)
        .map(|rid| (String::new(), rid.0));
    let routes = HashMap::from_iter(
        routes
            .into_iter()
            .map(|(s, rid)| (s, rid.0))
            // Include the default route if there is one
            .chain(default_route_iter)
            // Having the static_routes inserted after ensures that they will overwrite
            // any routes with the same prefix from the first set
            .chain(static_routes.into_iter().map(|(s, rid)| (s, rid.0))),
    );
    // TODO we may not want to print this because the routing table will be very big
    // if the node has a lot of local accounts
    trace!("Routing table is: {:?}", routes);
    *routing_table.write() = Arc::new(routes);
    Ok(())
}

// Uuid does not implement ToRedisArgs and FromRedisValue.
// Rust does not allow implementing foreign traits on foreign data types.
// As a result, we wrap Uuid in a local data type, and implement the necessary
// traits for that.
#[derive(Eq, PartialEq, Hash, Debug, Default, Serialize, Deserialize, Copy, Clone)]
struct RedisAccountId(Uuid);

impl FromStr for RedisAccountId {
    type Err = uuid::Error;

    fn from_str(src: &str) -> Result<Self, Self::Err> {
        let id = Uuid::from_str(&src)?;
        Ok(RedisAccountId(id))
    }
}

impl Display for RedisAccountId {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> Result<(), ::std::fmt::Error> {
        f.write_str(&self.0.to_hyphenated().to_string())
    }
}

impl ToRedisArgs for RedisAccountId {
    fn write_redis_args<W: RedisWrite + ?Sized>(&self, out: &mut W) {
        out.write_arg(self.0.to_hyphenated().to_string().as_bytes().as_ref());
    }
}

impl FromRedisValue for RedisAccountId {
    fn from_redis_value(v: &Value) -> Result<Self, RedisError> {
        let account_id = String::from_redis_value(v)?;
        let id = Uuid::from_str(&account_id)
            .map_err(|_| RedisError::from((ErrorKind::TypeError, "Invalid account id string")))?;
        Ok(RedisAccountId(id))
    }
}

impl ToRedisArgs for &AccountWithEncryptedTokens {
    fn write_redis_args<W: RedisWrite + ?Sized>(&self, out: &mut W) {
        let mut rv = Vec::with_capacity(ACCOUNT_DETAILS_FIELDS * 2);
        let account = &self.account;

        "id".write_redis_args(&mut rv);
        RedisAccountId(account.id).write_redis_args(&mut rv);
        "username".write_redis_args(&mut rv);
        account
            .username
            .as_bytes()
            .to_vec()
            .write_redis_args(&mut rv);
        if !account.ilp_address.is_empty() {
            "ilp_address".write_redis_args(&mut rv);
            rv.push(account.ilp_address.to_bytes().to_vec());
        }
        if !account.asset_code.is_empty() {
            "asset_code".write_redis_args(&mut rv);
            account.asset_code.write_redis_args(&mut rv);
        }
        "asset_scale".write_redis_args(&mut rv);
        account.asset_scale.write_redis_args(&mut rv);
        "max_packet_amount".write_redis_args(&mut rv);
        account.max_packet_amount.write_redis_args(&mut rv);
        "routing_relation".write_redis_args(&mut rv);
        account
            .routing_relation
            .to_string()
            .write_redis_args(&mut rv);
        "round_trip_time".write_redis_args(&mut rv);
        account.round_trip_time.write_redis_args(&mut rv);

        // Write optional fields
        if let Some(ilp_over_http_url) = account.ilp_over_http_url.as_ref() {
            "ilp_over_http_url".write_redis_args(&mut rv);
            ilp_over_http_url.as_str().write_redis_args(&mut rv);
        }
        if let Some(ilp_over_http_incoming_token) = account.ilp_over_http_incoming_token.as_ref() {
            "ilp_over_http_incoming_token".write_redis_args(&mut rv);
            ilp_over_http_incoming_token
                .expose_secret()
                .as_ref()
                .write_redis_args(&mut rv);
        }
        if let Some(ilp_over_http_outgoing_token) = account.ilp_over_http_outgoing_token.as_ref() {
            "ilp_over_http_outgoing_token".write_redis_args(&mut rv);
            ilp_over_http_outgoing_token
                .expose_secret()
                .as_ref()
                .write_redis_args(&mut rv);
        }
        if let Some(ilp_over_btp_url) = account.ilp_over_btp_url.as_ref() {
            "ilp_over_btp_url".write_redis_args(&mut rv);
            ilp_over_btp_url.as_str().write_redis_args(&mut rv);
        }
        if let Some(ilp_over_btp_incoming_token) = account.ilp_over_btp_incoming_token.as_ref() {
            "ilp_over_btp_incoming_token".write_redis_args(&mut rv);
            ilp_over_btp_incoming_token
                .expose_secret()
                .as_ref()
                .write_redis_args(&mut rv);
        }
        if let Some(ilp_over_btp_outgoing_token) = account.ilp_over_btp_outgoing_token.as_ref() {
            "ilp_over_btp_outgoing_token".write_redis_args(&mut rv);
            ilp_over_btp_outgoing_token
                .expose_secret()
                .as_ref()
                .write_redis_args(&mut rv);
        }
        if let Some(settle_threshold) = account.settle_threshold {
            "settle_threshold".write_redis_args(&mut rv);
            settle_threshold.write_redis_args(&mut rv);
        }
        if let Some(settle_to) = account.settle_to {
            "settle_to".write_redis_args(&mut rv);
            settle_to.write_redis_args(&mut rv);
        }
        if let Some(limit) = account.packets_per_minute_limit {
            "packets_per_minute_limit".write_redis_args(&mut rv);
            limit.write_redis_args(&mut rv);
        }
        if let Some(limit) = account.amount_per_minute_limit {
            "amount_per_minute_limit".write_redis_args(&mut rv);
            limit.write_redis_args(&mut rv);
        }
        if let Some(min_balance) = account.min_balance {
            "min_balance".write_redis_args(&mut rv);
            min_balance.write_redis_args(&mut rv);
        }
        if let Some(settlement_engine_url) = &account.settlement_engine_url {
            "settlement_engine_url".write_redis_args(&mut rv);
            settlement_engine_url.as_str().write_redis_args(&mut rv);
        }

        debug_assert!(rv.len() <= ACCOUNT_DETAILS_FIELDS * 2);
        debug_assert!((rv.len() % 2) == 0);

        ToRedisArgs::make_arg_vec(&rv, out);
    }
}

impl FromRedisValue for AccountWithEncryptedTokens {
    fn from_redis_value(v: &Value) -> Result<Self, RedisError> {
        let hash: HashMap<String, Value> = HashMap::from_redis_value(v)?;
        let ilp_address: String = get_value("ilp_address", &hash)?;
        let ilp_address = Address::from_str(&ilp_address)
            .map_err(|_| RedisError::from((ErrorKind::TypeError, "Invalid ILP address")))?;
        let username: String = get_value("username", &hash)?;
        let username = Username::from_str(&username)
            .map_err(|_| RedisError::from((ErrorKind::TypeError, "Invalid username")))?;
        let routing_relation: Option<String> = get_value_option("routing_relation", &hash)?;
        let routing_relation = if let Some(relation) = routing_relation {
            RoutingRelation::from_str(relation.as_str())
                .map_err(|_| RedisError::from((ErrorKind::TypeError, "Invalid Routing Relation")))?
        } else {
            RoutingRelation::NonRoutingAccount
        };
        let round_trip_time: Option<u32> = get_value_option("round_trip_time", &hash)?;
        let round_trip_time: u32 = round_trip_time.unwrap_or(DEFAULT_ROUND_TRIP_TIME);

        let rid: RedisAccountId = get_value("id", &hash)?;

        Ok(AccountWithEncryptedTokens {
            account: Account {
                id: rid.0,
                username,
                ilp_address,
                asset_code: get_value("asset_code", &hash)?,
                asset_scale: get_value("asset_scale", &hash)?,
                ilp_over_http_url: get_url_option("ilp_over_http_url", &hash)?,
                ilp_over_http_incoming_token: get_bytes_option(
                    "ilp_over_http_incoming_token",
                    &hash,
                )?
                .map(SecretBytesMut::from),
                ilp_over_http_outgoing_token: get_bytes_option(
                    "ilp_over_http_outgoing_token",
                    &hash,
                )?
                .map(SecretBytesMut::from),
                ilp_over_btp_url: get_url_option("ilp_over_btp_url", &hash)?,
                ilp_over_btp_incoming_token: get_bytes_option(
                    "ilp_over_btp_incoming_token",
                    &hash,
                )?
                .map(SecretBytesMut::from),
                ilp_over_btp_outgoing_token: get_bytes_option(
                    "ilp_over_btp_outgoing_token",
                    &hash,
                )?
                .map(SecretBytesMut::from),
                max_packet_amount: get_value("max_packet_amount", &hash)?,
                min_balance: get_value_option("min_balance", &hash)?,
                settle_threshold: get_value_option("settle_threshold", &hash)?,
                settle_to: get_value_option("settle_to", &hash)?,
                routing_relation,
                round_trip_time,
                packets_per_minute_limit: get_value_option("packets_per_minute_limit", &hash)?,
                amount_per_minute_limit: get_value_option("amount_per_minute_limit", &hash)?,
                settlement_engine_url: get_url_option("settlement_engine_url", &hash)?,
            },
        })
    }
}

fn get_value<V>(key: &str, map: &HashMap<String, Value>) -> Result<V, RedisError>
where
    V: FromRedisValue,
{
    if let Some(ref value) = map.get(key) {
        from_redis_value(value)
    } else {
        Err(RedisError::from((
            ErrorKind::TypeError,
            "Account is missing field",
            key.to_string(),
        )))
    }
}

fn get_value_option<V>(key: &str, map: &HashMap<String, Value>) -> Result<Option<V>, RedisError>
where
    V: FromRedisValue,
{
    if let Some(ref value) = map.get(key) {
        from_redis_value(value).map(Some)
    } else {
        Ok(None)
    }
}

fn get_bytes_option(
    key: &str,
    map: &HashMap<String, Value>,
) -> Result<Option<BytesMut>, RedisError> {
    if let Some(ref value) = map.get(key) {
        let vec: Vec<u8> = from_redis_value(value)?;
        Ok(Some(BytesMut::from(vec.as_slice())))
    } else {
        Ok(None)
    }
}

fn get_url_option(key: &str, map: &HashMap<String, Value>) -> Result<Option<Url>, RedisError> {
    if let Some(ref value) = map.get(key) {
        let value: String = from_redis_value(value)?;
        if let Ok(url) = Url::parse(&value) {
            Ok(Some(url))
        } else {
            Err(RedisError::from((ErrorKind::TypeError, "Invalid URL")))
        }
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis_crate::IntoConnectionInfo;

    #[tokio::test]
    async fn connect_fails_if_db_unavailable() {
        let result = RedisStoreBuilder::new(
            "redis://127.0.0.1:0".into_connection_info().unwrap() as ConnectionInfo,
            [0; 32],
        )
        .connect()
        .await;
        assert!(result.is_err());
    }
}
