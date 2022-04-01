use std::str::FromStr;

use cosmwasm_std::{
    to_binary, Addr, BankMsg, Coin, CosmosMsg, DepsMut, DistributionMsg, Env, MessageInfo, Order,
    Response, StdError, StdResult, SubMsg, SubMsgExecutionResponse, Uint128, WasmMsg,
};
use cw20::{Cw20ExecuteMsg, MinterResponse};
use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;
use terra_cosmwasm::{TerraMsg, TerraMsgWrapper, TerraRoute};

use crate::helpers::{query_cw20_total_supply, query_delegations};
use crate::math::{
    compute_delegations, compute_mint_amount, compute_unbond_amount, compute_undelegations,
};
use crate::msg::{Batch, CallbackMsg, ExecuteMsg, InstantiateMsg, PendingBatch, UnbondRequest};
use crate::state::State;
use crate::types::Coins;

//--------------------------------------------------------------------------------------------------
// Instantiation
//--------------------------------------------------------------------------------------------------

pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    let state = State::default();

    let worker_addrs = msg
        .workers
        .iter()
        .map(|s| deps.api.addr_validate(s))
        .collect::<StdResult<Vec<Addr>>>()?;

    state.epoch_period.save(deps.storage, &msg.epoch_period)?;
    state.unbond_period.save(deps.storage, &msg.unbond_period)?;
    state.workers.save(deps.storage, &worker_addrs)?;
    state.validators.save(deps.storage, &msg.validators)?;
    state.unlocked_coins.save(deps.storage, &vec![])?;

    state.pending_batch.save(
        deps.storage,
        &PendingBatch {
            id: 1,
            usteak_to_burn: Uint128::zero(),
            est_unbond_start_time: env.block.time.seconds() + msg.epoch_period,
        },
    )?;

    Ok(Response::new().add_submessage(SubMsg::reply_on_success(
        CosmosMsg::Wasm(WasmMsg::Instantiate {
            admin: Some(info.sender.into()), // for now we use the deployer as the admin
            code_id: msg.cw20_code_id,
            msg: to_binary(&Cw20InstantiateMsg {
                name: msg.name,
                symbol: msg.symbol,
                decimals: msg.decimals,
                initial_balances: vec![],
                mint: Some(MinterResponse {
                    minter: env.contract.address.into(),
                    cap: None,
                }),
                marketing: None,
            })?,
            funds: vec![],
            label: String::from("steak_token"),
        }),
        1,
    )))
}

pub fn register_steak_token(
    deps: DepsMut,
    response: SubMsgExecutionResponse,
) -> StdResult<Response> {
    let state = State::default();

    let event = response
        .events
        .iter()
        .find(|event| event.ty == "instantiate_contract")
        .ok_or_else(|| StdError::generic_err("cannot find `instantiate_contract` event"))?;

    let contract_addr_str = &event
        .attributes
        .iter()
        .find(|attr| attr.key == "contract_address")
        .ok_or_else(|| StdError::generic_err("cannot find `contract_address` attribute"))?
        .value;

    let contract_addr = deps.api.addr_validate(contract_addr_str)?;
    state.steak_token.save(deps.storage, &contract_addr)?;

    Ok(Response::new())
}

//--------------------------------------------------------------------------------------------------
// Bonding and harvesting logics
//--------------------------------------------------------------------------------------------------

pub fn bond(
    deps: DepsMut,
    env: Env,
    staker_addr: Addr,
    uluna_to_bond: Uint128,
) -> StdResult<Response<TerraMsgWrapper>> {
    let state = State::default();
    let steak_token = state.steak_token.load(deps.storage)?;
    let validators = state.validators.load(deps.storage)?;

    // Query the delegations made by Steak Hub to validators, as well as the total supply of Steak
    // token, which we will use to compute stuff
    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address)?;
    let usteak_supply = query_cw20_total_supply(&deps.querier, &steak_token)?;

    // Compute the amount of `usteak` to mint
    let usteak_to_mint = compute_mint_amount(usteak_supply, uluna_to_bond, &delegations);

    // Compute the amount of `uluna` to be delegated to each validator
    let new_delegations = compute_delegations(uluna_to_bond, &delegations);

    let delegate_submsgs: Vec<SubMsg<TerraMsgWrapper>> = new_delegations
        .iter()
        .map(|d| SubMsg::reply_on_success(d.to_cosmos_msg(), 2))
        .collect();

    let mint_msg: CosmosMsg<TerraMsgWrapper> = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: steak_token.into(),
        msg: to_binary(&Cw20ExecuteMsg::Mint {
            recipient: staker_addr.clone().into(),
            amount: usteak_to_mint,
        })?,
        funds: vec![],
    });

    Ok(Response::new()
        .add_submessages(delegate_submsgs)
        .add_message(mint_msg)
        .add_attribute("action", "steak_hub/bond")
        .add_attribute("staker", staker_addr)
        .add_attribute("uluna_bonded", uluna_to_bond))
}

pub fn harvest(deps: DepsMut, env: Env, worker_addr: Addr) -> StdResult<Response<TerraMsgWrapper>> {
    let state = State::default();

    // Only whitelisted workers can harvest
    let worker_addrs = state.workers.load(deps.storage)?;
    if !worker_addrs.contains(&worker_addr) {
        return Err(StdError::generic_err("sender is not a whitelisted worker"));
    }

    // For each of the whitelisted validators, create a message to withdraw delegation reward
    let delegate_submsgs: Vec<SubMsg<TerraMsgWrapper>> = deps
        .querier
        .query_all_delegations(&env.contract.address)?
        .into_iter()
        .map(|d| {
            SubMsg::reply_on_success(
                CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                    validator: d.validator,
                }),
                2,
            )
        })
        .collect();

    // Following the reward withdrawal, we dispatch two callbacks: to swap all rewards to Luna, and
    // to stake these Luna to the whitelisted validators
    let callback_msgs = vec![CallbackMsg::Swap {}, CallbackMsg::Reinvest {}]
        .iter()
        .map(|callback| callback.into_cosmos_msg(&env.contract.address))
        .collect::<StdResult<Vec<CosmosMsg<TerraMsgWrapper>>>>()?;

    Ok(Response::new()
        .add_submessages(delegate_submsgs)
        .add_messages(callback_msgs)
        .add_attribute("action", "steak_hub/harvest"))
}

pub fn swap(deps: DepsMut, _env: Env) -> StdResult<Response<TerraMsgWrapper>> {
    let state = State::default();
    let mut unlocked_coins = state.unlocked_coins.load(deps.storage)?;

    let coins_to_offer: Vec<Coin> =
        unlocked_coins.iter().cloned().filter(|coin| coin.denom != "uluna").collect();

    let swap_submsgs: Vec<SubMsg<TerraMsgWrapper>> = coins_to_offer
        .iter()
        .map(|coin| {
            SubMsg::reply_on_success(
                CosmosMsg::Custom(TerraMsgWrapper {
                    route: TerraRoute::Market,
                    msg_data: TerraMsg::Swap {
                        offer_coin: coin.clone(),
                        ask_denom: String::from("uluna"),
                    },
                }),
                2,
            )
        })
        .collect();

    unlocked_coins.retain(|coin| coin.denom == "uluna");
    state.unlocked_coins.save(deps.storage, &unlocked_coins)?;

    Ok(Response::new()
        .add_submessages(swap_submsgs)
        .add_attribute("action", "steak_hub/swap")
        .add_attribute("coins_offered", Coins(coins_to_offer).to_string()))
}

pub fn reinvest(deps: DepsMut, env: Env) -> StdResult<Response<TerraMsgWrapper>> {
    let state = State::default();
    let validators = state.validators.load(deps.storage)?;
    let mut unlocked_coins = state.unlocked_coins.load(deps.storage)?;

    let uluna_to_bond = unlocked_coins
        .iter()
        .find(|coin| coin.denom == "uluna")
        .ok_or_else(|| StdError::generic_err("no uluna available to be bonded"))?
        .amount;

    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address)?;
    let new_delegations = compute_delegations(uluna_to_bond, &delegations);

    unlocked_coins.retain(|coin| coin.denom == "uluna");
    state.unlocked_coins.save(deps.storage, &unlocked_coins)?;

    Ok(Response::new()
        .add_messages(new_delegations.iter().map(|d| d.to_cosmos_msg()))
        .add_attribute("action", "steak_hub/reinvest")
        .add_attribute("uluna_bonded", uluna_to_bond))
}

pub fn register_received_coins(
    deps: DepsMut,
    env: Env,
    response: SubMsgExecutionResponse,
) -> StdResult<Response> {
    let state = State::default();

    let event = response
        .events
        .iter()
        .find(|event| event.ty == "coin_received")
        .ok_or_else(|| StdError::generic_err("cannot find `coin_received` event"))?;

    let receiver = &event
        .attributes
        .iter()
        .find(|attr| attr.key == "receiver")
        .ok_or_else(|| StdError::generic_err("cannot find `receiver` attribute"))?
        .value;

    let coins_received_str = &event
        .attributes
        .iter()
        .find(|attr| attr.key == "amount")
        .ok_or_else(|| StdError::generic_err("cannot find `amount` attribute"))?
        .value;

    let coins_received = if *receiver == env.contract.address {
        Coins::from_str(coins_received_str)?
    } else {
        Coins(vec![])
    };

    state.unlocked_coins.update(deps.storage, |coins| -> StdResult<_> {
        let coins = Coins(coins).add_many(&coins_received)?;
        Ok(coins.0)
    })?;

    Ok(Response::new()
        .add_attribute("action", "steak_hub/register_unlocked_coins")
        .add_attribute("receiver", receiver)
        .add_attribute("coins_received", coins_received.to_string()))
}

//--------------------------------------------------------------------------------------------------
// Unbonding logics
//--------------------------------------------------------------------------------------------------

pub fn queue_unbond(
    deps: DepsMut,
    env: Env,
    staker_addr: Addr,
    usteak_to_burn: Uint128,
) -> StdResult<Response<TerraMsgWrapper>> {
    let state = State::default();

    // Update the pending batch data
    let mut pending_batch = state.pending_batch.load(deps.storage)?;
    pending_batch.usteak_to_burn += usteak_to_burn;
    state.pending_batch.save(deps.storage, &pending_batch)?;

    // Update the user's requested unbonding amount
    state.unbond_requests.update(
        deps.storage,
        (pending_batch.id.into(), &staker_addr),
        |x| -> StdResult<_> {
            let mut request = x.unwrap_or_else(|| UnbondRequest {
                id: pending_batch.id,
                user: staker_addr.to_string(),
                shares: Uint128::zero(),
            });
            request.shares += usteak_to_burn;
            Ok(request)
        },
    )?;

    // If the current batch's estimated unbonding start time is reached, then submit it for unbonding
    let mut msgs: Vec<CosmosMsg<TerraMsgWrapper>> = vec![];
    if env.block.time.seconds() >= pending_batch.est_unbond_start_time {
        msgs.push(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: env.contract.address.into(),
            msg: to_binary(&ExecuteMsg::SubmitBatch {})?,
            funds: vec![],
        }));
    }

    Ok(Response::new()
        .add_messages(msgs)
        .add_attribute("action", "steak_hub/queue_unbond")
        .add_attribute("staker", staker_addr)
        .add_attribute("usteak_to_burn", usteak_to_burn))
}

pub fn submit_batch(deps: DepsMut, env: Env) -> StdResult<Response<TerraMsgWrapper>> {
    let state = State::default();
    let steak_token = state.steak_token.load(deps.storage)?;
    let validators = state.validators.load(deps.storage)?;
    let unbond_period = state.unbond_period.load(deps.storage)?;
    let pending_batch = state.pending_batch.load(deps.storage)?;

    // The current batch can only be unbonded once the estimated unbonding time has been reached
    let current_time = env.block.time.seconds();
    if current_time < pending_batch.est_unbond_start_time {
        return Err(StdError::generic_err(
            format!("batch can only be submitted for unbonding after {}", pending_batch.est_unbond_start_time)
        ));
    }

    // Query the delegations made by Steak Hub to validators, as well as the total supply of Steak
    // token, which we will use to compute stuff
    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address)?;
    let usteak_supply = query_cw20_total_supply(&deps.querier, &steak_token)?;

    // Compute the amount of `uluna` to unbond
    let uluna_to_unbond = compute_unbond_amount(usteak_supply, pending_batch.usteak_to_burn, &delegations);

    // Compute the amount of `uluna` to undelegate from each validator
    let new_undelegations = compute_undelegations(uluna_to_unbond, &delegations);

    // Save the current pending batch to the previous batches map
    state.previous_batches.save(
        deps.storage,
        pending_batch.id.into(),
        &Batch {
            id: pending_batch.id,
            total_shares: pending_batch.usteak_to_burn,
            uluna_unclaimed: uluna_to_unbond,
            est_unbond_end_time: current_time + unbond_period,
        },
    )?;

    // Create the next pending batch
    let epoch_period = state.epoch_period.load(deps.storage)?;
    state.pending_batch.save(
        deps.storage,
        &PendingBatch {
            id: pending_batch.id + 1,
            usteak_to_burn: Uint128::zero(),
            est_unbond_start_time: current_time + epoch_period,
        },
    )?;

    let burn_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: steak_token.into(),
        msg: to_binary(&Cw20ExecuteMsg::Burn {
            amount: pending_batch.usteak_to_burn,
        })?,
        funds: vec![],
    });

    Ok(Response::new()
        .add_messages(new_undelegations.iter().map(|d| d.to_cosmos_msg()))
        .add_message(burn_msg)
        .add_attribute("action", "steak_hub/unbond")
        .add_attribute("batch_id", pending_batch.id.to_string())
        .add_attribute("usteak_burned", pending_batch.usteak_to_burn)
        .add_attribute("uluna_unbonded", uluna_to_unbond))
}

pub fn withdraw_unbonded(
    deps: DepsMut,
    env: Env,
    staker_addr: Addr,
) -> StdResult<Response<TerraMsgWrapper>> {
    let state = State::default();
    let current_time = env.block.time.seconds();

    // Fetch the user's unclaimed unbonding requests
    //
    // NOTE: If the user has too many unclaimed requests, this may not fit in the WASM memory... But
    // this practically is never going to happen in practice. Who would create hundreds of unbonding
    // requests and never claim them?
    let requests = state
        .unbond_requests
        .idx
        .user
        .prefix(staker_addr.to_string())
        .range(deps.storage, None, None, Order::Ascending)
        .map(|item| {
            let (_, v) = item?;
            Ok(v)
        })
        .collect::<StdResult<Vec<UnbondRequest>>>()?;

    // Enumerate through the user's all unclaimed unbonding requests. For each request, check whether
    // its batch has finished unbonding. It yes, increment the amount of uluna to refund the user,
    // and remove this request from the active queue
    //
    // If a batch has been completely refunded (i.e. total shares = 0), remove it from storage
    let mut total_uluna_to_refund = Uint128::zero();
    for request in &requests {
        let mut batch = state.previous_batches.load(deps.storage, request.id.into())?;
        if batch.est_unbond_end_time < current_time {
            let uluna_to_refund = batch.uluna_unclaimed.multiply_ratio(request.shares, batch.total_shares);

            total_uluna_to_refund += uluna_to_refund;
            batch.total_shares -= request.shares;
            batch.uluna_unclaimed -= uluna_to_refund;

            if batch.total_shares.is_zero() {
                state.previous_batches.remove(deps.storage, request.id.into());
            }

            state.unbond_requests.remove(deps.storage, (request.id.into(), &staker_addr))?;
        }
    }

    let refund_msg = CosmosMsg::Bank(BankMsg::Send {
        to_address: staker_addr.clone().into(),
        amount: vec![Coin::new(total_uluna_to_refund.u128(), "uluna")],
    });

    Ok(Response::new()
        .add_message(refund_msg)
        .add_attribute("action", "steak_hub/withdraw_unbonded")
        .add_attribute("staker", staker_addr)
        .add_attribute("uluna_refunded", total_uluna_to_refund))
}