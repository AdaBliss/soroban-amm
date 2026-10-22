#![no_std]

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, Address, Env, Vec,
};

const PRICE_SCALE: i128 = 1_000_000;
const POW_SCALE: i128 = 1_000_000_000_000;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdapterMode {
    Amm,
    ConcentratedLiquidity,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    Factory,
    Consumer,
    Mode,
    Window,
    MaxAge,
    FeeTiers,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AdapterError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidConfiguration = 3,
    PoolNotFound = 4,
    NoSnapshot = 5,
    StaleSnapshot = 6,
    InvalidPrice = 7,
    ArithmeticOverflow = 8,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TwapResult {
    pub twap: i128,
    pub window_used: u64,
    pub snapshot_timestamp: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClTwapResult {
    pub average_tick: i64,
    pub window_used: u64,
    pub snapshot_timestamp: u64,
}

#[contractclient(name = "FactoryClient")]
trait Factory {
    fn get_pool(env: Env, token_a: Address, token_b: Address) -> Option<Address>;
    fn get_cl_pool(
        env: Env,
        token_a: Address,
        token_b: Address,
        fee_bps: i128,
    ) -> Option<Address>;
    fn get_pool_tokens(env: Env, pool: Address) -> Option<(Address, Address)>;
}

#[contractclient(name = "TwapConsumerClient")]
trait TwapConsumer {
    fn get_twap_price_detailed(
        env: Env,
        pool: Address,
        window_seconds: u64,
    ) -> TwapResult;
    fn get_cl_twap_detailed(
        env: Env,
        pool: Address,
        window_seconds: u64,
    ) -> ClTwapResult;
    fn get_latest_snapshot_timestamp(env: Env, pool: Address) -> u64;
}

#[contractclient(name = "ClPoolClient")]
trait ClPool {
    fn active_liquidity(env: Env) -> i128;
}

#[contract]
pub struct OracleTwapAdapter;

#[contractimpl]
impl OracleTwapAdapter {
    pub fn initialize(
        env: Env,
        admin: Address,
        factory: Address,
        consumer: Address,
        mode: AdapterMode,
        window_seconds: u64,
        max_age_seconds: u64,
        fee_tiers: Vec<i128>,
    ) -> Result<(), AdapterError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(AdapterError::AlreadyInitialized);
        }
        if window_seconds == 0 || max_age_seconds == 0 {
            return Err(AdapterError::InvalidConfiguration);
        }
        if mode == AdapterMode::ConcentratedLiquidity && fee_tiers.is_empty() {
            return Err(AdapterError::InvalidConfiguration);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Factory, &factory);
        env.storage().instance().set(&DataKey::Consumer, &consumer);
        env.storage().instance().set(&DataKey::Mode, &mode);
        env.storage().instance().set(&DataKey::Window, &window_seconds);
        env.storage().instance().set(&DataKey::MaxAge, &max_age_seconds);
        env.storage().instance().set(&DataKey::FeeTiers, &fee_tiers);
        Ok(())
    }

    pub fn quote(
        env: Env,
        token_a: Address,
        token_b: Address,
    ) -> Result<(i128, u64), AdapterError> {
        let factory: Address = Self::read(&env, DataKey::Factory)?;
        let consumer: Address = Self::read(&env, DataKey::Consumer)?;
        let mode: AdapterMode = Self::read(&env, DataKey::Mode)?;
        let window: u64 = Self::read(&env, DataKey::Window)?;
        let max_age: u64 = Self::read(&env, DataKey::MaxAge)?;
        let factory_client = FactoryClient::new(&env, &factory);

        let pool = match mode {
            AdapterMode::Amm => factory_client
                .get_pool(&token_a, &token_b)
                .ok_or(AdapterError::PoolNotFound)?,
            AdapterMode::ConcentratedLiquidity => {
                let tiers: Vec<i128> = Self::read(&env, DataKey::FeeTiers)?;
                let mut selected: Option<(Address, i128)> = None;
                for tier in tiers.iter() {
                    if let Some(candidate) = factory_client.get_cl_pool(&token_a, &token_b, &tier) {
                        let liquidity = ClPoolClient::new(&env, &candidate).active_liquidity();
                        if selected.as_ref().map(|(_, l)| liquidity > *l).unwrap_or(true) {
                            selected = Some((candidate, liquidity));
                        }
                    }
                }
                selected.map(|(pool, _)| pool).ok_or(AdapterError::PoolNotFound)?
            }
        };

        let newest = TwapConsumerClient::new(&env, &consumer)
            .get_latest_snapshot_timestamp(&pool);
        if newest == 0 {
            return Err(AdapterError::NoSnapshot);
        }
        if env.ledger().timestamp().saturating_sub(newest) > max_age {
            return Err(AdapterError::StaleSnapshot);
        }

        let mut price = match mode {
            AdapterMode::Amm => TwapConsumerClient::new(&env, &consumer)
                .get_twap_price_detailed(&pool, &window)
                .twap,
            AdapterMode::ConcentratedLiquidity => {
                let result = TwapConsumerClient::new(&env, &consumer)
                    .get_cl_twap_detailed(&pool, &window);
                Self::tick_to_price(result.average_tick)?
            }
        };
        if price <= 0 {
            return Err(AdapterError::InvalidPrice);
        }

        let pool_tokens = factory_client
            .get_pool_tokens(&pool)
            .ok_or(AdapterError::PoolNotFound)?;
        if pool_tokens.0 != token_a {
            price = PRICE_SCALE
                .checked_mul(PRICE_SCALE)
                .ok_or(AdapterError::ArithmeticOverflow)?
                / price;
        }
        Ok((price, newest))
    }

    pub fn get_config(env: Env) -> Result<(Address, Address, AdapterMode, u64, u64), AdapterError> {
        Ok((
            Self::read(&env, DataKey::Factory)?,
            Self::read(&env, DataKey::Consumer)?,
            Self::read(&env, DataKey::Mode)?,
            Self::read(&env, DataKey::Window)?,
            Self::read(&env, DataKey::MaxAge)?,
        ))
    }

    fn read<T>(
        env: &Env,
        key: DataKey,
    ) -> Result<T, AdapterError>
    where
        T: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>,
        T::Error: core::fmt::Debug,
    {
        env.storage()
            .instance()
            .get(&key)
            .ok_or(AdapterError::NotInitialized)
    }

    fn tick_to_price(tick: i64) -> Result<i128, AdapterError> {
        let negative = tick < 0;
        let mut exponent = tick.unsigned_abs();
        let mut factor = 1_000_100_000_000_i128;
        let mut ratio = POW_SCALE;
        while exponent > 0 {
            if exponent & 1 == 1 {
                ratio = ratio
                    .checked_mul(factor)
                    .ok_or(AdapterError::ArithmeticOverflow)?
                    / POW_SCALE;
            }
            exponent >>= 1;
            if exponent > 0 {
                factor = factor
                    .checked_mul(factor)
                    .ok_or(AdapterError::ArithmeticOverflow)?
                    / POW_SCALE;
            }
        }
        if negative {
            ratio = POW_SCALE
                .checked_mul(POW_SCALE)
                .ok_or(AdapterError::ArithmeticOverflow)?
                / ratio;
        }
        ratio
            .checked_mul(PRICE_SCALE)
            .ok_or(AdapterError::ArithmeticOverflow)
            .map(|value| value / POW_SCALE)
    }
}
