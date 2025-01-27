use super::data_structures::{
    AMMContract, ExchangeContract, ExchangeContractConfiguration, LiquidityParameters,
};
use fuels::{
    prelude::{
        Address, AssetId, Bech32Address, Contract, ContractId, Provider, Salt, SettableContract,
        StorageConfiguration, TxParameters, WalletUnlocked,
    },
    tx::Contract as TxContract,
};

pub mod common {
    use super::*;
    use fuels::{
        programs::call_response::FuelCallResponse,
        test_helpers::{setup_multiple_assets_coins, setup_test_provider},
    };

    use crate::{
        data_structures::WalletAssetConfiguration,
        interface::{
            amm::initialize,
            exchange::{add_liquidity, constructor, deposit},
            Exchange, AMM,
        },
        paths::{
            AMM_CONTRACT_BINARY_PATH, AMM_CONTRACT_STORAGE_PATH, EXCHANGE_CONTRACT_BINARY_PATH,
            EXCHANGE_CONTRACT_STORAGE_PATH, MALICIOUS_EXCHANGE_CONTRACT_BINARY_PATH,
            MALICIOUS_EXCHANGE_CONTRACT_STORAGE_PATH,
        },
    };
    use std::collections::HashMap;

    pub async fn deploy_amm(wallet: &WalletUnlocked) -> AMMContract {
        let contract_id = Contract::deploy(
            AMM_CONTRACT_BINARY_PATH,
            wallet,
            TxParameters::default(),
            StorageConfiguration {
                storage_path: Some(AMM_CONTRACT_STORAGE_PATH.to_string()),
                manual_storage_vec: None,
            },
        )
        .await
        .unwrap();

        let instance = AMM::new(contract_id.clone(), wallet.clone());

        AMMContract {
            instance,
            id: contract_id.into(),
            pools: HashMap::new(),
        }
    }

    pub async fn deploy_and_construct_exchange(
        wallet: &WalletUnlocked,
        config: &ExchangeContractConfiguration,
    ) -> ExchangeContract {
        let (id, instance) = deploy_exchange(wallet, config).await;

        constructor(&instance, config.pair).await;

        ExchangeContract {
            bytecode_root: if config.compute_bytecode_root {
                Some(exchange_bytecode_root().await)
            } else {
                None
            },
            id,
            instance,
            pair: config.pair,
        }
    }

    pub async fn deploy_and_initialize_amm(wallet: &WalletUnlocked) -> AMMContract {
        let amm = deploy_amm(wallet).await;
        initialize(&amm.instance, exchange_bytecode_root().await).await;
        amm
    }

    pub async fn deploy_exchange(
        wallet: &WalletUnlocked,
        config: &ExchangeContractConfiguration,
    ) -> (ContractId, Exchange) {
        let binary_path = if config.malicious {
            MALICIOUS_EXCHANGE_CONTRACT_BINARY_PATH
        } else {
            EXCHANGE_CONTRACT_BINARY_PATH
        };
        let storage_path = if config.malicious {
            MALICIOUS_EXCHANGE_CONTRACT_STORAGE_PATH
        } else {
            EXCHANGE_CONTRACT_STORAGE_PATH
        }
        .to_string();

        let contract_id = Contract::deploy_with_parameters(
            binary_path,
            wallet,
            TxParameters::default(),
            StorageConfiguration {
                storage_path: Some(storage_path),
                manual_storage_vec: None,
            },
            Salt::from(config.salt),
        )
        .await
        .unwrap();

        let id = ContractId::from(contract_id.clone());
        let instance = Exchange::new(contract_id, wallet.clone());

        (id, instance)
    }

    pub async fn deposit_and_add_liquidity_with_response(
        liquidity_parameters: &LiquidityParameters,
        exchange: &ExchangeContract,
        override_gas_limit: bool,
    ) -> FuelCallResponse<u64> {
        deposit(
            &exchange.instance,
            liquidity_parameters.amounts.0,
            exchange.pair.0,
        )
        .await;

        deposit(
            &exchange.instance,
            liquidity_parameters.amounts.1,
            exchange.pair.1,
        )
        .await;

        add_liquidity(
            &exchange.instance,
            liquidity_parameters.liquidity,
            liquidity_parameters.deadline,
            override_gas_limit,
        )
        .await
    }

    // TODO: once the script is reliable enough, use it for this functionality
    pub async fn deposit_and_add_liquidity(
        liquidity_parameters: &LiquidityParameters,
        exchange: &ExchangeContract,
        override_gas_limit: bool,
    ) -> u64 {
        deposit_and_add_liquidity_with_response(liquidity_parameters, exchange, override_gas_limit)
            .await
            .value
    }

    pub async fn exchange_bytecode_root() -> ContractId {
        let exchange_raw_code = Contract::load_contract(
            EXCHANGE_CONTRACT_BINARY_PATH,
            &StorageConfiguration::default().storage_path,
        )
        .unwrap()
        .raw;
        (*TxContract::root_from_code(exchange_raw_code)).into()
    }

    pub async fn setup_wallet_and_provider(
        asset_parameters: &WalletAssetConfiguration,
    ) -> (WalletUnlocked, Vec<AssetId>, Provider) {
        let mut wallet = WalletUnlocked::new_random(None);

        let (coins, asset_ids) = setup_multiple_assets_coins(
            wallet.address(),
            asset_parameters.number_of_assets,
            asset_parameters.coins_per_asset,
            asset_parameters.amount_per_coin,
        );

        let (provider, _socket_addr) = setup_test_provider(coins.clone(), vec![], None, None).await;

        wallet.set_provider(provider.clone());

        (wallet, asset_ids, provider)
    }
}

pub mod scripts {
    use super::*;
    use crate::{data_structures::TransactionParameters, interface::amm::add_pool};
    use common::{deploy_and_construct_exchange, deposit_and_add_liquidity};
    use fuels::{
        tx::{Input, Output, TxPointer},
        types::resource::Resource,
    };

    pub const MAXIMUM_INPUT_AMOUNT: u64 = 1_000_000;

    pub fn contract_instances(amm: &AMMContract) -> Vec<&dyn SettableContract> {
        amm.pools
            .iter()
            .map(|((_, _), exchange)| &exchange.instance as &dyn SettableContract)
            .chain(std::iter::once(&amm.instance as &dyn SettableContract))
            .collect()
    }

    pub async fn setup_exchange_contract(
        wallet: &WalletUnlocked,
        exchange_config: &ExchangeContractConfiguration,
        liquidity_parameters: &LiquidityParameters,
    ) -> ExchangeContract {
        let exchange = deploy_and_construct_exchange(wallet, exchange_config).await;

        deposit_and_add_liquidity(liquidity_parameters, &exchange, false).await;

        exchange
    }

    pub async fn setup_exchange_contracts(
        wallet: &WalletUnlocked,
        provider: &Provider,
        amm: &mut AMMContract,
        asset_ids: &Vec<AssetId>,
    ) {
        let mut exchange_index = 0;

        while exchange_index < asset_ids.len() - 1 {
            // set exchanges so that there are pools for
            // (asset 1, asset 2), (asset 2, asset 3), (asset 3, asset 4) and so on
            let asset_pair = (
                *asset_ids.get(exchange_index).unwrap(),
                *asset_ids.get(exchange_index + 1).unwrap(),
            );

            let exchange = setup_exchange_contract(
                wallet,
                &ExchangeContractConfiguration::new(
                    Some(asset_pair),
                    None,
                    None,
                    // deploy identical contracts for different pools with salt
                    Some([(exchange_index as u8); 32]),
                ),
                &LiquidityParameters::new(
                    // add initial liquidity to exchanges to have ratios such as
                    // 1:1, 1:2, 1:3 and so on
                    Some((100_000, 100_000 * (exchange_index as u64 + 1))),
                    // a reasonable deadline for adding liquidity
                    Some(provider.latest_block_height().await.unwrap() + 10),
                    // liquidity that will be added is greater than or equal to the lowest deposit
                    Some(100_000),
                ),
            )
            .await;

            add_pool(&amm.instance, asset_pair, exchange.id).await;

            amm.pools.insert(asset_pair, exchange);
            exchange_index += 1;
        }
    }

    async fn transaction_input_coin(
        provider: &Provider,
        from: &Bech32Address,
        asset_id: AssetId,
        amount: u64,
    ) -> Vec<Input> {
        let coins = &provider
            .get_spendable_resources(from, asset_id, amount)
            .await
            .unwrap();

        let input_coins: Vec<Input> = coins
            .iter()
            .map(|coin| {
                let (coin_utxo_id, coin_amount) = match coin {
                    Resource::Coin(coin) => (coin.utxo_id, coin.amount),
                    _ => panic!("Resource type does not match"),
                };
                Input::CoinSigned {
                    utxo_id: coin_utxo_id,
                    owner: Address::from(from),
                    amount: coin_amount,
                    asset_id,
                    tx_pointer: TxPointer::default(),
                    witness_index: 0,
                    maturity: 0,
                }
            })
            .collect();

        input_coins
    }

    fn transaction_output_variable() -> Output {
        Output::Variable {
            amount: 0,
            to: Address::zeroed(),
            asset_id: AssetId::default(),
        }
    }

    pub async fn transaction_inputs_outputs(
        wallet: &WalletUnlocked,
        provider: &Provider,
        assets: &Vec<AssetId>,
        amounts: Option<&Vec<u64>>,
    ) -> TransactionParameters {
        let mut input_coins: Vec<Input> = vec![]; // capacity depends on wallet resources
        let mut output_variables: Vec<Output> = Vec::with_capacity(assets.len());

        for (asset_index, asset) in assets.iter().enumerate() {
            input_coins.extend(
                transaction_input_coin(
                    provider,
                    wallet.address(),
                    *asset,
                    if amounts.is_some() {
                        *amounts.unwrap().get(asset_index).unwrap()
                    } else {
                        MAXIMUM_INPUT_AMOUNT
                    },
                )
                .await,
            );
            output_variables.push(transaction_output_variable());
        }

        TransactionParameters {
            inputs: input_coins,
            outputs: output_variables,
        }
    }
}
