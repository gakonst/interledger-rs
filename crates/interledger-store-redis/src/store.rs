use super::account::*;
use bytes::Bytes;
use futures::{
    future::{err, ok, result, Either},
    Future, Stream,
};
use hashbrown::{HashMap, HashSet};
use interledger_api::{AccountDetails, NodeStore};
use interledger_btp::BtpStore;
use interledger_ccp::RouteManagerStore;
use interledger_http::HttpStore;
use interledger_router::RouterStore;
use interledger_service::{Account as AccountTrait, AccountStore};
use interledger_service_util::{BalanceStore, ExchangeRateStore};
use parking_lot::RwLock;
use redis::{self, cmd, r#async::SharedConnection, Client, PipelineCommands, Value};
use std::{
    iter::FromIterator,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_executor::spawn;
use tokio_timer::Interval;

const POLL_INTERVAL: u64 = 60000; // 1 minute

static ACCOUNT_FROM_INDEX: &str = "
local id = redis.call('HGET', KEYS[1], ARGV[1])
if not id then
    return nil
end
return redis.call('HGETALL', 'accounts:' .. id)";
static UPDATE_BALANCES: &str = "
local from_asset_code = string.lower(ARGV[1])
local from_id = ARGV[2]
local from_amount = tonumber(ARGV[3])
local to_asset_code = string.lower(ARGV[4])
local to_id = ARGV[5]
local to_amount = tonumber(ARGV[6])
local min_balance = redis.call('HGET', 'accounts:' .. from_id, 'min_balance')
if min_balance then
    min_balance = tonumber(min_balance)
    local balance = tonumber(redis.call('HGET', 'balances:' .. from_asset_code, from_id))
    if balance < min_balance + from_amount then
        error('Cannot subtract ' .. from_amount .. ' from balance. Current balance of account: ' .. from_id .. ' is: ' .. balance .. ' and min balance is: ' .. min_balance)
    end
end
local from_balance = redis.call('HINCRBY', 'balances:' .. from_asset_code, from_id, 0 - from_amount)
local to_balance = redis.call('HINCRBY', 'balances:' .. to_asset_code, to_id, to_amount)
return {from_balance, to_balance}";

static ROUTES_KEY: &str = "routes";
static RATES_KEY: &str = "rates";
static STATIC_ROUTES_KEY: &str = "routes:static";
static NEXT_ACCOUNT_ID_KEY: &str = "next_account_id";

fn account_details_key(account_id: u64) -> String {
    format!("accounts:{}", account_id)
}

fn balance_key(asset_code: &str) -> String {
    format!("balances:{}", asset_code.to_lowercase())
}

pub use redis::IntoConnectionInfo;

pub fn connect<R>(redis_uri: R) -> impl Future<Item = RedisStore, Error = ()>
where
    R: IntoConnectionInfo,
{
    connect_with_poll_interval(redis_uri, POLL_INTERVAL)
}

#[doc(hidden)]
pub fn connect_with_poll_interval<R>(
    redis_uri: R,
    poll_interval: u64,
) -> impl Future<Item = RedisStore, Error = ()>
where
    R: IntoConnectionInfo,
{
    result(Client::open(redis_uri))
        .map_err(|err| error!("Error creating Redis client: {:?}", err))
        .and_then(|client| {
            debug!("Connected to redis: {:?}", client);
            client
                .get_shared_async_connection()
                .map_err(|err| error!("Error connecting to Redis: {:?}", err))
        })
        .and_then(move |connection| {
            let store = RedisStore {
                connection: Arc::new(connection),
                exchange_rates: Arc::new(RwLock::new(HashMap::new())),
                routes: Arc::new(RwLock::new(HashMap::new())),
            };

            // Start polling for rate updates
            // Note: if this behavior changes, make sure to update the Drop implementation
            let connection_clone = Arc::downgrade(&store.connection);
            let exchange_rates = store.exchange_rates.clone();
            let poll_rates = Interval::new(Instant::now(), Duration::from_millis(poll_interval))
                .map_err(|err| error!("Interval error: {:?}", err))
                .for_each(move |_| {
                    if let Some(connection) = connection_clone.upgrade() {
                        Either::A(update_rates(
                            connection.as_ref().clone(),
                            exchange_rates.clone(),
                        ))
                    } else {
                        debug!("Not polling rates anymore because connection was closed");
                        // TODO make sure the interval stops
                        Either::B(err(()))
                    }
                });
            spawn(poll_rates);

            // Poll for routing table updates
            // Note: if this behavior changes, make sure to update the Drop implementation
            let connection_clone = Arc::downgrade(&store.connection);
            let routing_table = store.routes.clone();
            let poll_routes = Interval::new(Instant::now(), Duration::from_millis(poll_interval))
                .map_err(|err| error!("Interval error: {:?}", err))
                .for_each(move |_| {
                    if let Some(connection) = connection_clone.upgrade() {
                        Either::A(update_routes(
                            connection.as_ref().clone(),
                            routing_table.clone(),
                        ))
                    } else {
                        debug!("Not polling routes anymore because connection was closed");
                        // TODO make sure the interval stops
                        Either::B(err(()))
                    }
                });
            spawn(poll_routes);

            Ok(store)
        })
}

/// A Store that uses Redis as its underlying database.
///
/// This store leverages atomic Redis transactions to do operations such as balance updates.
///
/// Currently the RedisStore polls the database for the routing table and rate updates, but
/// future versions of it will use PubSub to subscribe to updates.
#[derive(Clone)]
pub struct RedisStore {
    connection: Arc<SharedConnection>,
    exchange_rates: Arc<RwLock<HashMap<String, f64>>>,
    routes: Arc<RwLock<HashMap<Bytes, u64>>>,
}

impl RedisStore {
    fn get_next_account_id(&self) -> impl Future<Item = u64, Error = ()> {
        cmd("INCR")
            .arg(NEXT_ACCOUNT_ID_KEY)
            .query_async(self.connection.as_ref().clone())
            .map_err(|err| error!("Error incrementing account ID: {:?}", err))
            .and_then(|(_conn, next_account_id): (_, u64)| Ok(next_account_id - 1))
    }
}

impl AccountStore for RedisStore {
    type Account = Account;

    // TODO cache results to avoid hitting Redis for each packet
    fn get_accounts(
        &self,
        account_ids: Vec<<Self::Account as AccountTrait>::AccountId>,
    ) -> Box<Future<Item = Vec<Account>, Error = ()> + Send> {
        let num_accounts = account_ids.len();
        let mut pipe = redis::pipe();
        for account_id in account_ids.iter() {
            pipe.cmd("HGETALL").arg(account_details_key(*account_id));
        }
        Box::new(
            pipe.query_async(self.connection.as_ref().clone())
                .map_err(move |err| {
                    error!(
                        "Error querying details for accounts: {:?} {:?}",
                        account_ids, err
                    )
                })
                .and_then(move |(_conn, accounts): (_, Vec<Account>)| {
                    if accounts.len() == num_accounts {
                        Ok(accounts)
                    } else {
                        Err(())
                    }
                }),
        )
    }
}

impl BalanceStore for RedisStore {
    fn get_balance(&self, account: Account) -> Box<Future<Item = i64, Error = ()> + Send> {
        Box::new(
            cmd("HGET")
                .arg(balance_key(account.asset_code.as_str()))
                .arg(account.id)
                .query_async(self.connection.as_ref().clone())
                .map_err(move |err| {
                    error!(
                        "Error getting balance for account: {} {:?}",
                        account.id, err
                    )
                })
                .and_then(|(_connection, balance): (_, i64)| Ok(balance)),
        )
    }

    fn update_balances(
        &self,
        from_account: Account,
        incoming_amount: u64,
        to_account: Account,
        outgoing_amount: u64,
    ) -> Box<Future<Item = (), Error = ()> + Send> {
        let from_account_id = from_account.id();
        let to_account_id = to_account.id();

        debug!(
            "Decreasing balance of account {} by: {}. Increasing balance of account {} by: {}",
            from_account_id, incoming_amount, to_account_id, outgoing_amount
        );

        Box::new(
            cmd("EVAL")
                // Update the balance only if it does not exceed the max_balance configured on the account
                .arg(UPDATE_BALANCES)
                .arg(0)
                .arg(from_account.asset_code)
                .arg(from_account_id)
                .arg(incoming_amount)
                .arg(to_account.asset_code)
                .arg(to_account_id)
                .arg(outgoing_amount)
                .query_async(self.connection.as_ref().clone())
                .map_err(move |err| {
                    error!(
                    "Error updating balances for accounts. from_account: {}, to_account: {}: {:?}",
                    from_account_id,
                    to_account_id,
                    err
                )
                })
                .and_then(
                    move |(_connection, (from_balance, to_balance)): (_, (i64, i64))| {
                        debug!(
                            "Updated account balances. Account {} has: {}, account {} has: {}",
                            from_account_id, from_balance, to_account_id, to_balance
                        );
                        Ok(())
                    },
                ),
        )
    }

    fn undo_balance_update(
        &self,
        from_account: Account,
        incoming_amount: u64,
        to_account: Account,
        outgoing_amount: u64,
    ) -> Box<Future<Item = (), Error = ()> + Send> {
        let from_account_id = from_account.id();
        let to_account_id = to_account.id();

        debug!(
            "Rolling back transaction. Increasing balance of account {} by: {}. Decreasing balance of account {} by: {}",
            from_account_id, incoming_amount, to_account_id, outgoing_amount
        );

        // TODO check against balance limit
        let mut pipe = redis::pipe();
        pipe.atomic()
            .cmd("HINCRBY")
            .arg(balance_key(from_account.asset_code.as_str()))
            .arg(from_account_id)
            .arg(incoming_amount as i64)
            .cmd("HINCRBY")
            .arg(balance_key(to_account.asset_code.as_str()))
            .arg(to_account_id)
            // TODO make sure this doesn't overflow
            .arg(0i64 - outgoing_amount as i64);

        Box::new(
            pipe.query_async(self.connection.as_ref().clone())
                .map_err(move |err| {
                    error!(
                    "Error undoing balance update for accounts. from_account: {}, to_account: {}: {:?}",
                    from_account_id,
                    to_account_id,
                    err
                )
                })
                .and_then(move |(_connection, balances): (_, Vec<i64>)| {
                    debug!(
                        "Updated account balances. Account {} has: {}, account {} has: {}",
                        from_account_id, balances[0], to_account_id, balances[1]
                    );
                    Ok(())
                }),
        )
    }
}

impl ExchangeRateStore for RedisStore {
    fn get_exchange_rates(&self, asset_codes: &[&str]) -> Result<Vec<f64>, ()> {
        let rates: Vec<f64> = asset_codes
            .iter()
            .filter_map(|code| {
                (*self.exchange_rates.read())
                    .get(&code.to_string())
                    .cloned()
            })
            .collect();
        if rates.len() == asset_codes.len() {
            Ok(rates)
        } else {
            Err(())
        }
    }
}

impl BtpStore for RedisStore {
    type Account = Account;

    fn get_account_from_btp_token(
        &self,
        token: &str,
    ) -> Box<Future<Item = Self::Account, Error = ()> + Send> {
        // TODO make sure it can't do script injection!
        // TODO cache the result so we don't hit redis for every packet (is that necessary if redis is often used as a cache?)
        let token = token.to_string();
        Box::new(
            cmd("EVAL")
                .arg(ACCOUNT_FROM_INDEX)
                .arg(1)
                .arg("btp_auth")
                .arg(&token)
                .query_async(self.connection.as_ref().clone())
                .map_err(|err| error!("Error getting account from BTP token: {:?}", err))
                .and_then(move |(_connection, account): (_, Option<Account>)| {
                    if let Some(account) = account {
                        Ok(account)
                    } else {
                        warn!("No account found with BTP token: {}", token);
                        Err(())
                    }
                }),
        )
    }
}

impl HttpStore for RedisStore {
    type Account = Account;

    fn get_account_from_http_auth(
        &self,
        auth_header: &str,
    ) -> Box<Future<Item = Self::Account, Error = ()> + Send> {
        // TODO make sure it can't do script injection!
        let auth_header = auth_header.to_string();
        Box::new(
            cmd("EVAL")
                .arg(ACCOUNT_FROM_INDEX)
                .arg(1)
                .arg("http_auth")
                .arg(&auth_header)
                .query_async(self.connection.as_ref().clone())
                .map_err(|err| error!("Error getting account from HTTP auth: {:?}", err))
                .and_then(move |(_connection, account): (_, Option<Account>)| {
                    if let Some(account) = account {
                        Ok(account)
                    } else {
                        warn!("No account found with HTTP auth: {}", auth_header);
                        Err(())
                    }
                }),
        )
    }
}

impl RouterStore for RedisStore {
    fn routing_table(&self) -> HashMap<Bytes, u64> {
        self.routes.read().clone()
    }
}

impl NodeStore for RedisStore {
    type Account = Account;

    fn insert_account(
        &self,
        account: AccountDetails,
    ) -> Box<Future<Item = Account, Error = ()> + Send> {
        debug!("Inserting account: {:?}", account);
        let connection = self.connection.clone();
        let routing_table = self.routes.clone();

        Box::new(
            self.get_next_account_id()
                .and_then(|id| {
                    debug!("Next account id is: {}", id);
                    Account::try_from(id, account)
                })
                .and_then(move |account| {
                    // Check that there isn't already an account with values that must be unique
                    let mut keys: Vec<String> = vec!["ID".to_string(), "ID".to_string()];

                    let mut pipe = redis::pipe();
                    pipe.cmd("EXISTS")
                        .arg(account_details_key(account.id))
                        .cmd("HEXISTS")
                        .arg(balance_key(account.asset_code.as_str()))
                        .arg(account.id);

                    if let Some(ref auth) = account.btp_incoming_authorization {
                        keys.push("BTP auth".to_string());
                        pipe.cmd("HEXISTS")
                            .arg("btp_auth")
                            .arg(auth.clone().to_string());
                    }
                    if let Some(ref auth) = account.http_incoming_authorization {
                        keys.push("HTTP auth".to_string());
                        pipe.cmd("HEXISTS")
                            .arg("http_auth")
                            .arg(auth.clone().to_string());
                    }
                    if let Some(ref xrp_address) = account.xrp_address {
                        keys.push("XRP address".to_string());
                        pipe.cmd("HEXISTS").arg("xrp_addresses").arg(xrp_address);
                    }

                    pipe.query_async(connection.as_ref().clone())
                        .map_err(|err| {
                            error!(
                                "Error checking whether account details already exist: {:?}",
                                err
                            )
                        })
                        .and_then(
                            move |(connection, results): (SharedConnection, Vec<bool>)| {
                                if let Some(index) = results.iter().position(|val| *val) {
                                    warn!("An account already exists with the same {}. Cannot insert account: {:?}", keys[index], account);
                                    Err(())
                                } else {
                                    Ok((connection, account))
                                }
                            },
                        )
                })
                .and_then(|(connection, account)| {
                    let mut pipe = redis::pipe();

                    // Set balance
                    pipe.atomic()
                        .cmd("HSET")
                        .arg(balance_key(account.asset_code.as_str()))
                        .arg(account.id)
                        .arg(0u64)
                        .ignore();

                    // Set incoming auth details
                    if let Some(ref auth) = account.btp_incoming_authorization {
                        pipe.cmd("HSET")
                            .arg("btp_auth")
                            .arg(auth.clone().to_string())
                            .arg(account.id)
                            .ignore();
                    }
                    if let Some(ref auth) = account.http_incoming_authorization {
                        pipe.cmd("HSET")
                            .arg("http_auth")
                            .arg(auth.clone().to_string())
                            .arg(account.id)
                            .ignore();
                    }

                    // Add settlement details
                    if let Some(ref xrp_address) = account.xrp_address {
                        pipe.cmd("HSET")
                            .arg("xrp_addresses")
                            .arg(xrp_address)
                            .arg(account.id)
                            .ignore();
                    }

                    if account.send_routes {
                        pipe.cmd("SADD")
                            .arg("send_routes_to")
                            .arg(account.id)
                            .ignore();
                    }

                    // Add route to routing table
                    pipe.hset(ROUTES_KEY, account.ilp_address.to_vec(), account.id)
                        .ignore();

                    // Set account details
                    pipe.cmd("HMSET")
                        .arg(account_details_key(account.id))
                        .arg(account.clone())
                        .ignore();

                    pipe.query_async(connection)
                        .map_err(|err| error!("Error inserting account into DB: {:?}", err))
                        .and_then(move |(connection, _ret): (SharedConnection, Value)| {
                            update_routes(connection, routing_table)
                        })
                        .and_then(move |_| Ok(account))
                }),
        )
    }

    // TODO limit the number of results and page through them
    fn get_all_accounts(&self) -> Box<Future<Item = Vec<Self::Account>, Error = ()> + Send> {
        Box::new(
            cmd("GET")
                .arg(NEXT_ACCOUNT_ID_KEY)
                .query_async(self.connection.as_ref().clone())
                .and_then(|(connection, next_account_id): (SharedConnection, u64)| {
                    let mut pipe = redis::pipe();
                    for i in 0..next_account_id {
                        pipe.cmd("HGETALL").arg(account_details_key(i));
                    }
                    pipe.query_async(connection)
                        .and_then(|(_, accounts): (_, Vec<Self::Account>)| Ok(accounts))
                })
                .map_err(|err| error!("Error getting all accounts: {:?}", err)),
        )
    }

    fn set_rates<R>(&self, rates: R) -> Box<Future<Item = (), Error = ()> + Send>
    where
        R: IntoIterator<Item = (String, f64)>,
    {
        let rates: Vec<(String, f64)> = rates.into_iter().collect();
        let exchange_rates = self.exchange_rates.clone();
        let mut pipe = redis::pipe();
        pipe.atomic()
            .cmd("DEL")
            .arg(RATES_KEY)
            .ignore()
            .cmd("HMSET")
            .arg(RATES_KEY)
            .arg(rates)
            .ignore();
        Box::new(
            pipe.query_async(self.connection.as_ref().clone())
                .map_err(|err| error!("Error setting rates: {:?}", err))
                .and_then(move |(connection, _): (SharedConnection, Value)| {
                    update_rates(connection, exchange_rates)
                }),
        )
    }

    // TODO fix inconsistency betwen this method and set_routes which
    // takes the prefixes as Bytes and the account as an Account object
    fn set_static_routes<R>(&self, routes: R) -> Box<Future<Item = (), Error = ()> + Send>
    where
        R: IntoIterator<Item = (String, u64)>,
    {
        let routes: Vec<(String, u64)> = routes.into_iter().collect();
        let accounts: HashSet<u64> =
            HashSet::from_iter(routes.iter().map(|(_prefix, account_id)| *account_id));
        let mut pipe = redis::pipe();
        for account_id in accounts {
            pipe.cmd("EXISTS").arg(account_details_key(account_id));
        }

        let routing_table = self.routes.clone();
        Box::new(pipe.query_async(self.connection.as_ref().clone())
            .map_err(|err| error!("Error checking if accounts exist while setting static routes: {:?}", err))
            .and_then(|(connection, accounts_exist): (SharedConnection, Vec<bool>)| {
                if accounts_exist.iter().all(|a| *a) {
                    Ok(connection)
                } else {
                    error!("Error setting static routes because not all of the given accounts exist");
                    Err(())
                }
            })
            .and_then(move |connection| {
        let mut pipe = redis::pipe();
        pipe.atomic()
            .cmd("DEL")
            .arg(STATIC_ROUTES_KEY)
            .ignore()
            .cmd("HMSET")
            .arg(STATIC_ROUTES_KEY)
            .arg(routes)
            .ignore();
            pipe.query_async(connection)
                .map_err(|err| error!("Error setting static routes: {:?}", err))
                .and_then(move |(connection, _): (SharedConnection, Value)| {
                    update_routes(connection, routing_table)
                })
            }))
    }

    fn set_static_route(
        &self,
        prefix: String,
        account_id: u64,
    ) -> Box<Future<Item = (), Error = ()> + Send> {
        let routing_table = self.routes.clone();
        let prefix_clone = prefix.clone();
        Box::new(
        cmd("EXISTS")
            .arg(account_details_key(account_id))
            .query_async(self.connection.as_ref().clone())
            .map_err(|err| error!("Error checking if account exists before setting static route: {:?}", err))
            .and_then(move |(connection, exists): (SharedConnection, bool)| {
                if exists {
                    Ok(connection)
                } else {
                    error!("Cannot set static route for prefix: {} because account {} does not exist", prefix_clone, account_id);
                    Err(())
                }
            })
            .and_then(move |connection| {
                cmd("HSET")
                    .arg(STATIC_ROUTES_KEY)
                    .arg(prefix)
                    .arg(account_id)
                    .query_async(connection)
                    .map_err(|err| error!("Error setting static route: {:?}", err))
                    .and_then(move |(connection, _): (SharedConnection, Value)| {
                        update_routes(connection, routing_table)
                    })
            })
        )
    }
}

impl RouteManagerStore for RedisStore {
    type Account = Account;

    fn get_accounts_to_send_routes_to(
        &self,
    ) -> Box<Future<Item = Vec<Account>, Error = ()> + Send> {
        Box::new(
            cmd("SMEMBERS")
                .arg("send_routes_to")
                .query_async(self.connection.as_ref().clone())
                .map_err(|err| error!("Error getting members of set send_routes_to: {:?}", err))
                .and_then(|(connection, account_ids): (SharedConnection, Vec<u64>)| {
                    if account_ids.is_empty() {
                        Either::A(ok(Vec::new()))
                    } else {
                        let mut pipe = redis::pipe();
                        for id in account_ids {
                            pipe.cmd("HGETALL").arg(account_details_key(id));
                        }
                        Either::B(
                            pipe.query_async(connection)
                                .map_err(|err| {
                                    error!("Error getting accounts to send routes to: {:?}", err)
                                })
                                .and_then(
                                    |(_connection, accounts): (SharedConnection, Vec<Account>)| {
                                        Ok(accounts)
                                    },
                                ),
                        )
                    }
                }),
        )
    }

    fn get_local_and_configured_routes(
        &self,
    ) -> Box<Future<Item = ((HashMap<Bytes, Account>), (HashMap<Bytes, Account>)), Error = ()> + Send>
    {
        let get_static_routes = cmd("HGETALL")
            .arg(STATIC_ROUTES_KEY)
            .query_async(self.connection.as_ref().clone())
            .map_err(|err| error!("Error getting static routes: {:?}", err))
            .and_then(
                |(_, static_routes): (SharedConnection, Vec<(String, u64)>)| Ok(static_routes),
            );
        Box::new(self.get_all_accounts().join(get_static_routes).and_then(
            |(accounts, static_routes)| {
                let local_table = HashMap::from_iter(
                    accounts
                        .iter()
                        .map(|account| (account.ilp_address.clone(), account.clone())),
                );

                let account_map: HashMap<u64, &Account> = HashMap::from_iter(accounts.iter().map(|account| (account.id, account)));
                let configured_table: HashMap<Bytes, Account> = HashMap::from_iter(static_routes.into_iter()
                    .filter_map(|(prefix, account_id)| {
                        if let Some(account) = account_map.get(&account_id) {
                            Some((Bytes::from(prefix), (*account).clone()))
                        } else {
                            warn!("No account for ID: {}, ignoring configured route for prefix: {}", account_id, prefix);
                            None
                        }
                    }));

                Ok((local_table, configured_table))
            },
        ))
    }

    fn set_routes<R>(&mut self, routes: R) -> Box<Future<Item = (), Error = ()> + Send>
    where
        R: IntoIterator<Item = (Bytes, Account)>,
    {
        let routes: Vec<(String, u64)> = routes
            .into_iter()
            .filter_map(|(prefix, account)| {
                if let Ok(prefix) = String::from_utf8(prefix.to_vec()) {
                    Some((prefix, account.id))
                } else {
                    None
                }
            })
            .collect();
        let num_routes = routes.len();

        // Save routes to Redis
        let routing_tale = self.routes.clone();
        let mut pipe = redis::pipe();
        pipe.atomic()
            .cmd("DEL")
            .arg(ROUTES_KEY)
            .ignore()
            .cmd("HMSET")
            .arg(ROUTES_KEY)
            .arg(routes)
            .ignore();
        Box::new(
            pipe.query_async(self.connection.as_ref().clone())
                .map_err(|err| error!("Error setting routes: {:?}", err))
                .and_then(move |(connection, _): (SharedConnection, Value)| {
                    trace!("Saved {} routes to Redis", num_routes);
                    update_routes(connection, routing_tale)
                }),
        )
    }
}

// TODO replace this with pubsub when async pubsub is added upstream: https://github.com/mitsuhiko/redis-rs/issues/183
fn update_rates(
    connection: SharedConnection,
    exchange_rates: Arc<RwLock<HashMap<String, f64>>>,
) -> impl Future<Item = (), Error = ()> {
    cmd("HGETALL")
        .arg(RATES_KEY)
        .query_async(connection)
        .map_err(|err| error!("Error polling for exchange rates: {:?}", err))
        .and_then(move |(_connection, rates): (_, Vec<(String, f64)>)| {
            let num_assets = rates.len();
            let rates = HashMap::from_iter(rates.into_iter());
            (*exchange_rates.write()) = rates;
            debug!("Updated rates for {} assets", num_assets);
            Ok(())
        })
}

// TODO replace this with pubsub when async pubsub is added upstream: https://github.com/mitsuhiko/redis-rs/issues/183
type RouteVec = Vec<(String, u64)>;

fn update_routes(
    connection: SharedConnection,
    routing_table: Arc<RwLock<HashMap<Bytes, u64>>>,
) -> impl Future<Item = (), Error = ()> {
    let mut pipe = redis::pipe();
    pipe.cmd("HGETALL")
        .arg(ROUTES_KEY)
        .cmd("HGETALL")
        .arg(STATIC_ROUTES_KEY);
    pipe.query_async(connection)
        .map_err(|err| error!("Error polling for routing table updates: {:?}", err))
        .and_then(
            move |(_connection, (routes, static_routes)): (_, (RouteVec, RouteVec))| {
                trace!(
                    "Loaded routes from redis. Static routes: {:?}, other routes: {:?}",
                    static_routes,
                    routes
                );
                let routes = HashMap::from_iter(
                    routes
                        .into_iter()
                        // Having the static_routes inserted after ensures that they will overwrite
                        // any routes with the same prefix from the first set
                        .chain(static_routes.into_iter())
                        .map(|(prefix, account_id)| (Bytes::from(prefix), account_id)),
                );
                trace!("Routing table is now: {:?}", routes);
                let num_routes = routes.len();
                *routing_table.write() = routes;
                debug!("Updated routing table with {} routes", num_routes);
                Ok(())
            },
        )
}
