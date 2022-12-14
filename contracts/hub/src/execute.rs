use std::cmp::Ordering;
use std::convert::TryInto;
use std::ops::Mul;
use std::str::FromStr;

use cosmwasm_std::{
    to_binary, Addr, BankMsg, Coin, CosmosMsg, Decimal, Decimal256, DepsMut, Env, Event, Order,
    Response, StdError, StdResult, Storage, SubMsg, SubMsgResponse, Uint128, Uint64, WasmMsg,
};
use cw20::{Cw20ExecuteMsg, MinterResponse};
use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;
use sha2::{Digest, Sha256};

use crate::contract::{REPLY_INSTANTIATE_TOKEN, REPLY_REGISTER_RECEIVED_COINS};
use pfc_steak::hub::{
    Batch, CallbackMsg, ExecuteMsg, FeeType, InstantiateMsg, PendingBatch, UnbondRequest,
};
use pfc_steak::DecimalCheckedOps;

use crate::helpers::{
    get_denom_balance, parse_received_fund, query_cw20_total_supply, query_delegation,
    query_delegations,
};
use crate::math::{
    compute_mint_amount, compute_redelegations_for_rebalancing, compute_redelegations_for_removal,
    compute_target_delegation_from_mining_power, compute_unbond_amount, compute_undelegations,
    reconcile_batches,
};
use crate::state::State;
use crate::types::{Coins, Delegation, RewardWithdrawal};

// minimum amount of time it should take to mine a block (20 seconds)
pub const TARGET_MINING_DURATION_FLOOR_SECONDS: u64 = 20u64;
// maximum amount of time it should take to mine a block (5 minutes)
pub const TARGET_MINING_DURATION_CEILING_SECONDS: u64 = 300u64;

//--------------------------------------------------------------------------------------------------
// Instantiation
//--------------------------------------------------------------------------------------------------

pub fn instantiate(deps: DepsMut, env: Env, msg: InstantiateMsg) -> StdResult<Response> {
    let state = State::default();

    if msg.max_fee_amount > Decimal::from_str("1.00")? {
        return Err(StdError::generic_err("Max fee can not exceed 1/100%"));
    }

    if msg.fee_amount > msg.max_fee_amount {
        return Err(StdError::generic_err("fee can not exceed max fee"));
    }
    let fee_type = FeeType::from_str(&msg.fee_account_type)
        .map_err(|_| StdError::generic_err("Invalid Fee type: Wallet or FeeSplit only"))?;

    state
        .owner
        .save(deps.storage, &deps.api.addr_validate(&msg.owner)?)?;
    state.epoch_period.save(deps.storage, &msg.epoch_period)?;
    state.unbond_period.save(deps.storage, &msg.unbond_period)?;
    state.validators.save(deps.storage, &msg.validators)?;
    state.unlocked_coins.save(deps.storage, &vec![])?;
    state.prev_denom.save(deps.storage, &Uint128::zero())?;
    state.denom.save(deps.storage, &msg.denom)?;
    state.max_fee_rate.save(deps.storage, &msg.max_fee_amount)?;
    state.fee_rate.save(deps.storage, &msg.fee_amount)?;
    state.fee_account_type.save(deps.storage, &fee_type)?;

    state
        .fee_account
        .save(deps.storage, &deps.api.addr_validate(&msg.fee_account)?)?;

    state.pending_batch.save(
        deps.storage,
        &PendingBatch {
            id: 1,
            usteak_to_burn: Uint128::zero(),
            est_unbond_start_time: env.block.time.seconds() + msg.epoch_period,
        },
    )?;
    state
        .validators_active
        .save(deps.storage, &msg.validators)?;

    state.miner_entropy.save(
        deps.storage,
        // arbitrary entropy
        &env.contract.address.to_string(),
    )?;
    state.miner_entropy_draft.save(
        deps.storage,
        // arbitrary entropy
        &env.contract.address.to_string(),
    )?;

    // difficulty starts at one
    state.miner_difficulty.save(deps.storage, &1u64.into())?;
    // last mined block starts at current timestamp
    state
        .miner_last_mined_timestamp
        .save(deps.storage, &env.block.time.seconds().into())?;
    // last mined block starts at current block height
    state
        .miner_last_mined_block
        .save(deps.storage, &env.block.height.into())?;
    // total mining power starts at zero
    state
        .total_mining_power
        .save(deps.storage, &Uint128::zero())?;

    Ok(Response::new().add_submessage(SubMsg::reply_on_success(
        CosmosMsg::Wasm(WasmMsg::Instantiate {
            admin: Some(msg.owner), // use the owner as admin for now; can be changed later by a `MsgUpdateAdmin`
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
                marketing: msg.marketing,
            })?,
            funds: vec![],
            label: msg.label.unwrap_or_else(|| "steak_token".to_string()),
        }),
        REPLY_INSTANTIATE_TOKEN,
    )))
}

pub fn register_steak_token(deps: DepsMut, response: SubMsgResponse) -> StdResult<Response> {
    let state = State::default();

    let event = response
        .events
        .iter()
        .find(|event| event.ty == "instantiate")
        .ok_or_else(|| StdError::generic_err("cannot find `instantiate` event"))?;

    let contract_addr_str = &event
        .attributes
        .iter()
        .find(|attr| attr.key == "_contract_address")
        .ok_or_else(|| StdError::generic_err("cannot find `_contract_address` attribute"))?
        .value;

    let contract_addr = deps.api.addr_validate(contract_addr_str)?;
    state.steak_token.save(deps.storage, &contract_addr)?;

    Ok(Response::new())
}

//--------------------------------------------------------------------------------------------------
// Bonding and harvesting logics
//--------------------------------------------------------------------------------------------------

/// NOTE: In a previous implementation, we split up the deposited Native Token over all validators, so that
/// they all have the same amount of delegation. This is however quite gas-expensive: $1.5 cost in
/// the case of 15 validators.
///
/// To save gas for users, now we simply delegate all deposited Native Token to the validator with the
/// smallest amount of delegation. If delegations become severely unbalance as a result of this
/// (e.g. when a single user makes a very big deposit), anyone can invoke `ExecuteMsg::Rebalance`
/// to balance the delegations.
pub fn bond(deps: DepsMut, env: Env, receiver: Addr, funds: Vec<Coin>) -> StdResult<Response> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let amount_to_bond = parse_received_fund(&funds, &denom)?;
    let steak_token = state.steak_token.load(deps.storage)?;
    let validators = state.validators_active.load(deps.storage)?;

    // Query the current delegations made to validators, and find the validator with the smallest
    // delegated amount through a linear search
    // The code for linear search is a bit uglier than using `sort_by` but cheaper: O(n) vs O(n * log(n))
    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address, &denom)?;
    let mut validator = &delegations[0].validator;
    let mut amount = delegations[0].amount;
    for d in &delegations[1..] {
        if d.amount < amount {
            validator = &d.validator;
            amount = d.amount;
        }
    }
    let new_delegation = Delegation {
        validator: validator.clone(),
        amount: amount_to_bond.u128(),
        denom: denom.clone(),
    };

    // Query the current supply of Steak and compute the amount to mint
    let usteak_supply = query_cw20_total_supply(&deps.querier, &steak_token)?;
    let usteak_to_mint = compute_mint_amount(usteak_supply, amount_to_bond, &delegations);
    state.prev_denom.save(
        deps.storage,
        &get_denom_balance(&deps.querier, env.contract.address.clone(), denom.clone())?,
    )?;

    let delegate_submsg = SubMsg::reply_on_success(
        new_delegation.to_cosmos_msg(env.contract.address.to_string())?,
        REPLY_REGISTER_RECEIVED_COINS,
    );

    let mint_msg: CosmosMsg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: steak_token.into(),
        msg: to_binary(&Cw20ExecuteMsg::Mint {
            recipient: receiver.to_string(),
            amount: usteak_to_mint,
        })?,
        funds: vec![],
    });

    let event = Event::new("steakhub/bonded")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("receiver", receiver)
        .add_attribute("denom_bonded", denom)
        .add_attribute("denom_amount", amount_to_bond)
        .add_attribute("usteak_minted", usteak_to_mint);

    Ok(Response::new()
        .add_submessage(delegate_submsg)
        .add_message(mint_msg)
        .add_event(event)
        .add_attribute("action", "steakhub/bond"))
}

pub fn harvest(deps: DepsMut, env: Env, sender: Addr) -> StdResult<Response> {
    if sender != env.contract.address {
        return Err(StdError::generic_err(
            "only the contract itself can harvest rewards for DPOW",
        ));
    }
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    state.prev_denom.save(
        deps.storage,
        &get_denom_balance(&deps.querier, env.contract.address.clone(), denom)?,
    )?;

    let withdraw_submsgs = deps
        .querier
        .query_all_delegations(&env.contract.address)?
        .into_iter()
        .map(|d| -> StdResult<SubMsg> {
            Ok(SubMsg::reply_on_success(
                RewardWithdrawal {
                    validator: d.validator,
                }
                .to_cosmos_msg(env.contract.address.to_string())?,
                REPLY_REGISTER_RECEIVED_COINS,
            ))
        })
        .collect::<StdResult<Vec<SubMsg>>>()?;

    let callback_msg = CallbackMsg::Reinvest {}.into_cosmos_msg(&env.contract.address)?;

    Ok(Response::new()
        .add_submessages(withdraw_submsgs)
        .add_message(callback_msg)
        .add_attribute("action", "steakhub/harvest"))
}

/// NOTE:
/// 1. When delegation Native denom here, we don't need to use a `SubMsg` to handle the received coins,
/// because we have already withdrawn all claimable staking rewards previously in the same atomic
/// execution.
/// 2. Same as with `bond`, in the latest implementation we only delegate staking rewards with the
/// validator that has the smallest delegation amount.
pub fn reinvest(deps: DepsMut, env: Env) -> StdResult<Response> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let fee = state.fee_rate.load(deps.storage)?;

    let validators = state.validators_active.load(deps.storage)?;
    let prev_coin = state.prev_denom.load(deps.storage)?;
    let current_coin =
        get_denom_balance(&deps.querier, env.contract.address.clone(), denom.clone())?;

    if current_coin <= prev_coin {
        return Err(StdError::generic_err("no rewards"));
    }
    let amount_to_bond = current_coin.saturating_sub(prev_coin);
    let mut unlocked_coins = state.unlocked_coins.load(deps.storage)?;

    /*

        if unlocked_coins.is_empty() {
            return Err(StdError::generic_err("no rewards"));
        }
        let amount_to_bond = unlocked_coins
            .iter()
            .find(|coin| coin.denom == denom)
            .ok_or_else(|| StdError::generic_err("no native amount available to be bonded"))?
            .amount;
    */
    let total_mining_power = state
        .total_mining_power
        .may_load(deps.storage)?
        .unwrap_or_default();
    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address, &denom)?;
    let total_bonded = delegations.iter().fold(0u128, |acc, d| acc + d.amount);
    let mut validator = &delegations[0].validator;
    let validator_mining_power = state
        .validator_mining_powers
        .may_load(deps.storage, validator.to_string())?
        .unwrap_or_default();
    let target_delegation = compute_target_delegation_from_mining_power(
        total_bonded.into(),
        validator_mining_power,
        total_mining_power,
    )?;
    println!(
        "total mining power: {} total bonded: {}",
        total_mining_power, total_bonded
    );

    let mut cmp = target_delegation.u128().cmp(&delegations[0].amount);
    let mut diff = if cmp.is_gt() {
        target_delegation.u128().abs_diff(delegations[0].amount)
    } else {
        0u128
    };
    println!(
        "validator: {} amount: {} target: {} diff: {}",
        validator,
        delegations[0].amount,
        target_delegation.u128(),
        diff
    );

    for d in &delegations[1..] {
        let current_validator_mining_power = state
            .validator_mining_powers
            .may_load(deps.storage, d.validator.to_string())?
            .unwrap_or_default();
        let current_td = compute_target_delegation_from_mining_power(
            total_bonded.into(),
            current_validator_mining_power,
            total_mining_power,
        )?;
        let current_diff = current_td.u128().abs_diff(d.amount);
        println!(
            "validator: {} amount: {} target: {} diff: {}",
            d.validator,
            d.amount,
            current_td.u128(),
            current_diff
        );
        let current_cmp = current_td.u128().cmp(&d.amount);
        // if there is a bigger gap to fill with the current validator, use it
        if current_cmp > cmp || (current_cmp.is_gt() && current_diff > diff) {
            validator = &d.validator;
            diff = current_diff;
            cmp = current_cmp;
        }
    }
    let fee_amount = if fee.is_zero() {
        Uint128::zero()
    } else {
        fee.checked_mul_uint(amount_to_bond)?
    };
    let amount_to_bond_minus_fees = amount_to_bond.saturating_sub(fee_amount);

    let new_delegation = Delegation::new(validator, amount_to_bond_minus_fees.u128(), &denom);

    unlocked_coins.retain(|coin| coin.denom != denom);
    state.unlocked_coins.save(deps.storage, &unlocked_coins)?;

    let event = Event::new("steakhub/harvested")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("denom", &denom)
        .add_attribute("fees_deducted", fee_amount)
        .add_attribute("denom_bonded", amount_to_bond_minus_fees);

    if fee_amount > Uint128::zero() {
        let fee_account = state.fee_account.load(deps.storage)?;
        let fee_type = state.fee_account_type.load(deps.storage)?;

        let send_msgs = match fee_type {
            FeeType::Wallet => vec![CosmosMsg::Bank(BankMsg::Send {
                to_address: fee_account.to_string(),
                amount: vec![Coin::new(fee_amount.into(), &denom)],
            })],
            FeeType::FeeSplit => {
                let msg = pfc_fee_split::fee_split_msg::ExecuteMsg::Deposit { flush: false };

                vec![msg.into_cosmos_msg(fee_account, vec![Coin::new(fee_amount.into(), &denom)])?]
            }
        };
        Ok(Response::new()
            .add_message(new_delegation.to_cosmos_msg(env.contract.address.to_string())?)
            .add_messages(send_msgs)
            .add_event(event)
            .add_attribute("action", "steakhub/reinvest"))
    } else {
        Ok(Response::new()
            .add_message(new_delegation.to_cosmos_msg(env.contract.address.to_string())?)
            .add_event(event)
            .add_attribute("action", "steakhub/reinvest"))
    }
}

/// NOTE: a `SubMsgResponse` may contain multiple coin-receiving events, must handle them individually
pub fn register_received_coins(
    deps: DepsMut,
    env: Env,
    mut events: Vec<Event>,
) -> StdResult<Response> {
    events.retain(|event| event.ty == "coin_received");
    if events.is_empty() {
        return Ok(Response::new());
    }

    let mut received_coins = Coins(vec![]);
    for event in &events {
        received_coins.add_many(&parse_coin_receiving_event(&env, event)?)?;
    }

    let state = State::default();
    state
        .unlocked_coins
        .update(deps.storage, |coins| -> StdResult<_> {
            let mut coins = Coins(coins);
            coins.add_many(&received_coins)?;
            Ok(coins.0)
        })?;

    Ok(Response::new().add_attribute("action", "steakhub/register_received_coins"))
}

fn parse_coin_receiving_event(env: &Env, event: &Event) -> StdResult<Coins> {
    let receiver = &event
        .attributes
        .iter()
        .find(|attr| attr.key == "receiver")
        .ok_or_else(|| StdError::generic_err("cannot find `receiver` attribute"))?
        .value;

    let amount_str = &event
        .attributes
        .iter()
        .find(|attr| attr.key == "amount")
        .ok_or_else(|| StdError::generic_err("cannot find `amount` attribute"))?
        .value;

    let amount = if *receiver == env.contract.address {
        Coins::from_str(amount_str)?
    } else {
        Coins(vec![])
    };

    Ok(amount)
}

//--------------------------------------------------------------------------------------------------
// Unbonding logics
//--------------------------------------------------------------------------------------------------

pub fn queue_unbond(
    deps: DepsMut,
    env: Env,
    receiver: Addr,
    usteak_to_burn: Uint128,
) -> StdResult<Response> {
    let state = State::default();

    let mut pending_batch = state.pending_batch.load(deps.storage)?;
    pending_batch.usteak_to_burn += usteak_to_burn;
    state.pending_batch.save(deps.storage, &pending_batch)?;

    state.unbond_requests.update(
        deps.storage,
        (pending_batch.id, &receiver),
        |x| -> StdResult<_> {
            let mut request = x.unwrap_or_else(|| UnbondRequest {
                id: pending_batch.id,
                user: receiver.clone(),
                shares: Uint128::zero(),
            });
            request.shares += usteak_to_burn;
            Ok(request)
        },
    )?;

    let mut msgs: Vec<CosmosMsg> = vec![];
    if env.block.time.seconds() >= pending_batch.est_unbond_start_time {
        msgs.push(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: env.contract.address.into(),
            msg: to_binary(&ExecuteMsg::SubmitBatch {})?,
            funds: vec![],
        }));
    }

    let event = Event::new("steakhub/unbond_queued")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("id", pending_batch.id.to_string())
        .add_attribute("receiver", receiver)
        .add_attribute("usteak_to_burn", usteak_to_burn);

    Ok(Response::new()
        .add_messages(msgs)
        .add_event(event)
        .add_attribute("action", "steakhub/queue_unbond"))
}

pub fn submit_batch(deps: DepsMut, env: Env) -> StdResult<Response> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let steak_token = state.steak_token.load(deps.storage)?;
    let validators = state.validators.load(deps.storage)?;
    let unbond_period = state.unbond_period.load(deps.storage)?;
    let pending_batch = state.pending_batch.load(deps.storage)?;

    let current_time = env.block.time.seconds();
    if current_time < pending_batch.est_unbond_start_time {
        return Err(StdError::generic_err(format!(
            "batch can only be submitted for unbonding after {}",
            pending_batch.est_unbond_start_time
        )));
    }

    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address, &denom)?;
    let usteak_supply = query_cw20_total_supply(&deps.querier, &steak_token)?;

    let amount_to_bond =
        compute_unbond_amount(usteak_supply, pending_batch.usteak_to_burn, &delegations);
    let new_undelegations = compute_undelegations(amount_to_bond, &delegations, &denom);

    // NOTE: Regarding the `amount_unclaimed` value
    //
    // If validators misbehave and get slashed during the unbonding period, the contract can receive
    // LESS Native Token than `amount_to_unbond` when unbonding finishes!
    //
    // In this case, users who invokes `withdraw_unbonded` will have their txs failed as the contract
    // does not have enough Native Token balance.
    //
    // I don't have a solution for this... other than to manually fund contract with the slashed amount.
    state.previous_batches.save(
        deps.storage,
        pending_batch.id,
        &Batch {
            id: pending_batch.id,
            reconciled: false,
            total_shares: pending_batch.usteak_to_burn,
            amount_unclaimed: amount_to_bond,
            est_unbond_end_time: current_time + unbond_period,
        },
    )?;

    let epoch_period = state.epoch_period.load(deps.storage)?;
    state.pending_batch.save(
        deps.storage,
        &PendingBatch {
            id: pending_batch.id + 1,
            usteak_to_burn: Uint128::zero(),
            est_unbond_start_time: current_time + epoch_period,
        },
    )?;
    state.prev_denom.save(
        deps.storage,
        &get_denom_balance(&deps.querier, env.contract.address.clone(), denom)?,
    )?;

    let undelegate_submsgs = new_undelegations
        .iter()
        .map(|d| {
            Ok(SubMsg::reply_on_success(
                d.to_cosmos_msg(env.contract.address.to_string())?,
                REPLY_REGISTER_RECEIVED_COINS,
            ))
        })
        .collect::<StdResult<Vec<_>>>()?;

    let burn_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: steak_token.into(),
        msg: to_binary(&Cw20ExecuteMsg::Burn {
            amount: pending_batch.usteak_to_burn,
        })?,
        funds: vec![],
    });

    let event = Event::new("steakhub/unbond_submitted")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("id", pending_batch.id.to_string())
        .add_attribute("native_unbonded", amount_to_bond)
        .add_attribute("usteak_burned", pending_batch.usteak_to_burn);

    Ok(Response::new()
        .add_submessages(undelegate_submsgs)
        .add_message(burn_msg)
        .add_event(event)
        .add_attribute("action", "steakhub/unbond"))
}

pub fn reconcile(deps: DepsMut, env: Env) -> StdResult<Response> {
    let state = State::default();
    let current_time = env.block.time.seconds();

    // Load batches that have not been reconciled
    let all_batches = state
        .previous_batches
        .idx
        .reconciled
        .prefix(false.into())
        .range(deps.storage, None, None, Order::Ascending)
        .map(|item| {
            let (_, v) = item?;
            Ok(v)
        })
        .collect::<StdResult<Vec<_>>>()?;

    let mut batches = all_batches
        .into_iter()
        .filter(|b| current_time > b.est_unbond_end_time)
        .collect::<Vec<_>>();

    let native_expected_received: Uint128 = batches.iter().map(|b| b.amount_unclaimed).sum();
    let denom = state.denom.load(deps.storage)?;
    let unlocked_coins = state.unlocked_coins.load(deps.storage)?;

    let native_expected_unlocked = Coins(unlocked_coins).find(&denom).amount;

    let native_expected = native_expected_received + native_expected_unlocked;
    let native_actual = deps
        .querier
        .query_balance(&env.contract.address, &denom)?
        .amount;

    let native_to_deduct = native_expected
        .checked_sub(native_actual)
        .unwrap_or_else(|_| Uint128::zero());
    if !native_to_deduct.is_zero() {
        reconcile_batches(&mut batches, native_expected - native_actual);
    }

    for batch in batches.iter_mut() {
        batch.reconciled = true;
        state.previous_batches.save(deps.storage, batch.id, batch)?;
    }

    let ids = batches
        .iter()
        .map(|b| b.id.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let event = Event::new("steakhub/reconciled")
        .add_attribute("ids", ids)
        .add_attribute("native_deducted", native_to_deduct.to_string());

    Ok(Response::new()
        .add_event(event)
        .add_attribute("action", "steakhub/reconcile"))
}
pub fn withdraw_unbonded_admin(
    deps: DepsMut,
    env: Env,
    user: Addr,
    receiver: Addr,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &user)?;

    withdraw_unbonded(deps, env, receiver.clone(), receiver)
}

pub fn withdraw_unbonded(
    deps: DepsMut,
    env: Env,
    user: Addr,
    receiver: Addr,
) -> StdResult<Response> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let current_time = env.block.time.seconds();

    // NOTE: If the user has too many unclaimed requests, this may not fit in the WASM memory...
    // However, this is practically never going to happen. Who would create hundreds of unbonding
    // requests and never claim them?
    let requests = state
        .unbond_requests
        .idx
        .user
        .prefix(user.to_string())
        .range(deps.storage, None, None, Order::Ascending)
        .map(|item| {
            let (_, v) = item?;
            Ok(v)
        })
        .collect::<StdResult<Vec<_>>>()?;

    // NOTE: Native in the following batches are withdrawn it the batch:
    // - is a _previous_ batch, not a _pending_ batch
    // - is reconciled
    // - has finished unbonding
    // If not sure whether the batches have been reconciled, the user should first invoke `ExecuteMsg::Reconcile`
    // before withdrawing.
    let mut total_native_to_refund = Uint128::zero();
    let mut ids: Vec<String> = vec![];
    for request in &requests {
        if let Ok(mut batch) = state.previous_batches.load(deps.storage, request.id) {
            if batch.reconciled && batch.est_unbond_end_time < current_time {
                let native_to_refund = batch
                    .amount_unclaimed
                    .multiply_ratio(request.shares, batch.total_shares);

                ids.push(request.id.to_string());

                total_native_to_refund += native_to_refund;
                batch.total_shares -= request.shares;
                batch.amount_unclaimed -= native_to_refund;

                if batch.total_shares.is_zero() {
                    state.previous_batches.remove(deps.storage, request.id)?;
                } else {
                    state
                        .previous_batches
                        .save(deps.storage, batch.id, &batch)?;
                }

                state
                    .unbond_requests
                    .remove(deps.storage, (request.id, &user))?;
            }
        }
    }

    if total_native_to_refund.is_zero() {
        return Err(StdError::generic_err("withdrawable amount is zero"));
    }

    let refund_msg = CosmosMsg::Bank(BankMsg::Send {
        to_address: receiver.clone().into(),
        amount: vec![Coin::new(total_native_to_refund.u128(), &denom)],
    });

    let event = Event::new("steakhub/unbonded_withdrawn")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("ids", ids.join(","))
        .add_attribute("user", user)
        .add_attribute("receiver", receiver)
        .add_attribute("amount_refunded", total_native_to_refund);

    Ok(Response::new()
        .add_message(refund_msg)
        .add_event(event)
        .add_attribute("action", "steakhub/withdraw_unbonded"))
}

//--------------------------------------------------------------------------------------------------
// Ownership and management logics
//--------------------------------------------------------------------------------------------------

pub fn rebalance(deps: DepsMut, env: Env, minimum: Uint128) -> StdResult<Response> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let validators = state.validators.load(deps.storage)?;
    let validators_active = state.validators_active.load(deps.storage)?;

    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address, &denom)?;

    let total_delegated_amount = delegations.iter().fold(0u128, |acc, d| acc + d.amount);

    let total_mining_power = state.total_mining_power.load(deps.storage)?;

    let new_redelegations =
        compute_redelegations_for_rebalancing(validators_active, &delegations, minimum, |d| {
            compute_target_delegation_from_mining_power(
                total_delegated_amount.into(),
                state
                    .validator_mining_powers
                    .may_load(deps.storage, d.validator.clone())?
                    .unwrap_or_default(),
                total_mining_power,
            )
        })?;

    state.prev_denom.save(
        deps.storage,
        &get_denom_balance(&deps.querier, env.contract.address.clone(), denom)?,
    )?;

    let redelegate_submsgs = new_redelegations
        .iter()
        .map(|rd| {
            Ok(SubMsg::reply_on_success(
                rd.to_cosmos_msg(env.contract.address.to_string())?,
                REPLY_REGISTER_RECEIVED_COINS,
            ))
        })
        .collect::<StdResult<Vec<_>>>()?;

    let amount: u128 = new_redelegations.iter().map(|rd| rd.amount).sum();

    let event = Event::new("steakhub/rebalanced").add_attribute("amount_moved", amount.to_string());

    Ok(Response::new()
        .add_submessages(redelegate_submsgs)
        .add_event(event)
        .add_attribute("action", "steakhub/rebalance"))
}

pub fn add_validator(deps: DepsMut, sender: Addr, validator: String) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;

    state.validators.update(deps.storage, |mut validators| {
        if validators.contains(&validator) {
            return Err(StdError::generic_err("validator is already whitelisted"));
        }
        validators.push(validator.clone());
        Ok(validators)
    })?;

    let mut validators_active = state.validators_active.load(deps.storage)?;
    if !validators_active.contains(&validator) {
        validators_active.push(validator.clone());
    }
    state
        .validators_active
        .save(deps.storage, &validators_active)?;
    let event = Event::new("steakhub/validator_added").add_attribute("validator", validator);

    Ok(Response::new()
        .add_event(event)
        .add_attribute("action", "steakhub/add_validator"))
}

pub fn remove_validator(
    deps: DepsMut,
    env: Env,
    sender: Addr,
    validator: String,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    let denom = state.denom.load(deps.storage)?;

    let validators = state.validators.update(deps.storage, |mut validators| {
        if !validators.contains(&validator) {
            return Err(StdError::generic_err(
                "validator is not already whitelisted",
            ));
        }
        validators.retain(|v| *v != validator);
        Ok(validators)
    })?;
    let mut validators_active = state.validators_active.load(deps.storage)?;
    if !validators_active.contains(&validator) {
        validators_active.push(validator.clone());
    }
    state
        .validators_active
        .save(deps.storage, &validators_active)?;

    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address, &denom)?;
    let delegation_to_remove =
        query_delegation(&deps.querier, &validator, &env.contract.address, &denom)?;
    let new_redelegations =
        compute_redelegations_for_removal(&delegation_to_remove, &delegations, &denom);

    state.prev_denom.save(
        deps.storage,
        &get_denom_balance(&deps.querier, env.contract.address.clone(), denom)?,
    )?;

    let redelegate_submsgs = new_redelegations
        .iter()
        .map(|d| {
            Ok(SubMsg::reply_on_success(
                d.to_cosmos_msg(env.contract.address.to_string())?,
                REPLY_REGISTER_RECEIVED_COINS,
            ))
        })
        .collect::<StdResult<Vec<_>>>()?;

    let event = Event::new("steak/validator_removed").add_attribute("validator", validator);

    Ok(Response::new()
        .add_submessages(redelegate_submsgs)
        .add_event(event)
        .add_attribute("action", "steakhub/remove_validator"))
}

pub fn remove_validator_ex(
    deps: DepsMut,
    _env: Env,
    sender: Addr,
    validator: String,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;

    state.validators.update(deps.storage, |mut validators| {
        if !validators.contains(&validator) {
            return Err(StdError::generic_err(
                "validator is not already whitelisted",
            ));
        }
        validators.retain(|v| *v != validator);
        Ok(validators)
    })?;

    let event = Event::new("steak/validator_removed_ex").add_attribute("validator", validator);

    Ok(Response::new()
        .add_event(event)
        .add_attribute("action", "steakhub/remove_validator_ex"))
}

pub fn pause_validator(
    deps: DepsMut,
    _env: Env,
    sender: Addr,
    validator: String,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;

    state
        .validators_active
        .update(deps.storage, |mut validators| {
            if !validators.contains(&validator) {
                return Err(StdError::generic_err(
                    "validator is not already whitelisted",
                ));
            }
            validators.retain(|v| *v != validator);
            Ok(validators)
        })?;

    let event = Event::new("steak/pause_validator").add_attribute("validator", validator);

    Ok(Response::new()
        .add_event(event)
        .add_attribute("action", "steakhub/pause_validator"))
}

pub fn unpause_validator(
    deps: DepsMut,
    _env: Env,
    sender: Addr,
    validator: String,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    let mut validators_active = state.validators_active.load(deps.storage)?;
    if !validators_active.contains(&validator) {
        validators_active.push(validator.clone());
    }
    state
        .validators_active
        .save(deps.storage, &validators_active)?;

    let event = Event::new("steak/unpause_validator").add_attribute("validator", validator);

    Ok(Response::new()
        .add_event(event)
        .add_attribute("action", "steakhub/unpause_validator"))
}
pub fn set_unbond_period(
    deps: DepsMut,
    _env: Env,
    sender: Addr,
    unbond_period: u64,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    state.unbond_period.save(deps.storage, &unbond_period)?;
    let event = Event::new("steak/set_unbond_period")
        .add_attribute("unbond_period", format!("{}", unbond_period));

    Ok(Response::new()
        .add_event(event)
        .add_attribute("action", "steakhub/set_unbond_period"))
}

pub fn transfer_ownership(deps: DepsMut, sender: Addr, new_owner: String) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    state
        .new_owner
        .save(deps.storage, &deps.api.addr_validate(&new_owner)?)?;

    Ok(Response::new().add_attribute("action", "steakhub/transfer_ownership"))
}

pub fn accept_ownership(deps: DepsMut, sender: Addr) -> StdResult<Response> {
    let state = State::default();

    let previous_owner = state.owner.load(deps.storage)?;
    let new_owner = state.new_owner.load(deps.storage)?;

    if sender != new_owner {
        return Err(StdError::generic_err(
            "unauthorized: sender is not new owner",
        ));
    }

    state.owner.save(deps.storage, &sender)?;
    state.new_owner.remove(deps.storage);

    let event = Event::new("steakhub/ownership_transferred")
        .add_attribute("new_owner", new_owner)
        .add_attribute("previous_owner", previous_owner);

    Ok(Response::new()
        .add_event(event)
        .add_attribute("action", "steakhub/transfer_ownership"))
}

fn transfer_fee_account_internal(
    deps: DepsMut,
    fee_account_type: String,
    new_fee_account: String,
) -> StdResult<()> {
    let state = State::default();
    let fee_type = FeeType::from_str(&fee_account_type)
        .map_err(|_| StdError::generic_err("Invalid Fee type: Wallet or FeeSplit only"))?;
    state.fee_account_type.save(deps.storage, &fee_type)?;
    state
        .fee_account
        .save(deps.storage, &deps.api.addr_validate(&new_fee_account)?)?;
    Ok(())
}

pub fn transfer_fee_account(
    deps: DepsMut,
    sender: Addr,
    fee_account_type: String,
    new_fee_account: String,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;

    transfer_fee_account_internal(deps, fee_account_type, new_fee_account)?;

    Ok(Response::new().add_attribute("action", "steakhub/transfer_fee_account"))
}

pub fn change_denom(deps: DepsMut, sender: Addr, new_denom: String) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    state.denom.save(deps.storage, &new_denom)?;

    Ok(Response::new().add_attribute("action", "steakhub/change_denom"))
}

pub fn update_fee(deps: DepsMut, sender: Addr, new_fee: Decimal) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    if new_fee > state.max_fee_rate.load(deps.storage)? {
        return Err(StdError::generic_err(
            "refusing to set fee above maximum set",
        ));
    }
    state.fee_rate.save(deps.storage, &new_fee)?;

    Ok(Response::new().add_attribute("action", "steakhub/update_fee"))
}

// update entropy execute function
pub fn update_entropy(
    deps: DepsMut,
    env: Env,
    _sender: Addr,
    entropy: String,
) -> StdResult<Response> {
    let state = State::default();

    let next_entropy =
        state
            .miner_entropy_draft
            .update(deps.storage, |entropy_draft| -> StdResult<String> {
                // use sha2 to hash the entropy
                let mut hasher = Sha256::new();
                hasher.update(entropy_draft);
                hasher.update(entropy);
                let result = hasher.finalize();
                // convert the hash to a hex string
                let entropy_hash = hex::encode(result);
                // convert bytes to string
                let entropy_hash = String::from_utf8(entropy_hash.as_bytes().to_vec())?;
                Ok(entropy_hash)
            })?;

    update_difficulty(deps.storage, env.block.time.seconds(), false)?;

    Ok(Response::new()
        .add_attribute("action", "steakhub/update_entropy")
        .add_attribute("miner_entropy_draft", next_entropy))
}

pub fn create_difficulty_prefix(difficulty: Uint64) -> String {
    // validate difficulty
    let mut difficulty_string = String::new();
    for _ in 0..difficulty.u64() {
        difficulty_string.push('0');
    }
    difficulty_string
}

#[test]
fn test_create_difficulty_prefix() {
    let difficulty = Uint64::from(3u64);
    let difficulty_string = create_difficulty_prefix(difficulty);
    assert_eq!(difficulty_string, "000");
    let difficulty = Uint64::from(1u64);
    let difficulty_string = create_difficulty_prefix(difficulty);
    assert_eq!(difficulty_string, "0");
}

pub fn compute_miner_proof(
    miner_entropy: &str,
    miner_address: &str,
    nonce: Uint64,
) -> StdResult<String> {
    // validate block hash
    let mut hasher = Sha256::new();
    hasher.update(&miner_entropy);
    hasher.update(miner_address);
    hasher.update(nonce.to_le_bytes());
    let result = hasher.finalize();
    let entropy_hash = hex::encode(result);
    let entropy_hash = String::from_utf8(entropy_hash.as_bytes().to_vec())?;

    Ok(entropy_hash)
}
// unit test for compute_miner_proof
#[test]
fn test_compute_miner_proof() {
    let miner_entropy = "abcdefg".to_string();
    let miner_address = "cosmos123".to_string();
    let nonce = Uint64::from(3825297897467829464u64);
    let result = compute_miner_proof(&miner_entropy, &miner_address, nonce);
    assert_eq!(
        result.unwrap(),
        "eb7d03dd856d797aea48b2a080357810c50b366d2a40fd358e1f1b18d3a62d5c"
    );
}

pub fn update_difficulty(
    store: &mut dyn Storage,
    block_time: u64,
    did_submit_proof: bool,
) -> StdResult<()> {
    let state = State::default();
    let miner_last_mined_timestamp = state.miner_last_mined_timestamp.load(store)?;
    let difficulty = state.miner_difficulty.load(store)?;
    // update mining difficulty based on the mining duration ceiling and floor
    let mining_duration = block_time - miner_last_mined_timestamp.u64();

    // update difficulty
    if mining_duration > TARGET_MINING_DURATION_CEILING_SECONDS && difficulty.u64() > 1 {
        // too hard to mine, decrease difficulty
        state
            .miner_difficulty
            .update(store, |difficulty| -> StdResult<Uint64> {
                Ok(difficulty.checked_sub(1u64.into())?)
            })?;
    // we only allow difficulty to increase if a proof was submitted
    } else if mining_duration < TARGET_MINING_DURATION_FLOOR_SECONDS && did_submit_proof {
        // too easy to mine, increase difficulty
        state
            .miner_difficulty
            .update(store, |difficulty| -> StdResult<Uint64> {
                Ok(difficulty.checked_add(1u64.into())?)
            })?;
    }
    Ok(())
}

// submit proof execute function
// * validates block hash of entropy + sender bech32 + sender nonce meets the required mining difficulty
// * sets miner_entropy to equal a hash of the block hash and miner_entropy_draft
// * sets fee address to sender,
// * executes Rebalance {} cosmwasm message on itself
pub fn submit_proof(
    deps: DepsMut,
    env: Env,
    sender: Addr,
    nonce: Uint64,
    validator_address: String,
) -> StdResult<Response> {
    let state = State::default();
    let validator = deps
        .querier
        .query_validator(validator_address)?
        .ok_or_else(|| StdError::generic_err("validator address not found in staking module"))?;
    let miner_entropy = state.miner_entropy.load(deps.storage)?;
    let miner_entropy_draft = state.miner_entropy_draft.load(deps.storage)?;
    let fee_account_type = state.fee_account_type.load(deps.storage)?;
    let difficulty = state.miner_difficulty.load(deps.storage)?;
    let miner_last_mined_block = state
        .miner_last_mined_block
        .load(deps.storage)
        // defaults to previous block height
        .or_else(|_| -> StdResult<Uint64> { Ok(Uint64::from(env.block.height - 1)) })?;

    let entropy_hash = compute_miner_proof(&miner_entropy, &sender.to_string(), nonce)?;

    let difficulty_string = create_difficulty_prefix(difficulty);

    if !entropy_hash.starts_with(&difficulty_string) {
        return Err(StdError::generic_err(
            "block hash does not meet difficulty requirement",
        ));
    }
    // compute hash of miner_entropy_draft and entropy_hash
    let mut hasher = Sha256::new();
    hasher.update(&miner_entropy_draft);
    hasher.update(&entropy_hash);
    let result = hasher.finalize();
    let miner_entropy = hex::encode(result);
    let miner_entropy = String::from_utf8(miner_entropy.as_bytes().to_vec())?;

    // blocks since last mined block
    let mining_duration_blocks = env.block.height - miner_last_mined_block.u64();

    update_difficulty(deps.storage, env.block.time.seconds(), true)?;

    // update validator mining power
    state.validator_mining_powers.update(
        deps.storage,
        validator.address,
        |mining_power| -> StdResult<Uint128> {
            Ok(mining_power
                .unwrap_or_default()
                .checked_add(Uint128::from(mining_duration_blocks))
                .map_err(StdError::overflow)?)
        },
    )?;

    // update total mining power
    state
        .total_mining_power
        .update(deps.storage, |total_mining_power| -> StdResult<Uint128> {
            Ok(total_mining_power
                .checked_add(Uint128::from(mining_duration_blocks))
                .map_err(StdError::overflow)?)
        })?;

    // set miner entropy
    state.miner_entropy.save(deps.storage, &miner_entropy)?;

    // set miner entropy draft to the entropy hash
    state
        .miner_entropy_draft
        .save(deps.storage, &entropy_hash)?;

    // set last mined timestamp
    state
        .miner_last_mined_timestamp
        .save(deps.storage, &env.block.time.seconds().into())?;

    // set last mined block
    state
        .miner_last_mined_block
        .save(deps.storage, &env.block.height.into())?;

    // set fee account
    if fee_account_type != FeeType::Wallet {
        state
            .fee_account_type
            .save(deps.storage, &FeeType::Wallet)?;
    }
    // make the miner the fee recipient
    state.fee_account.save(deps.storage, &sender)?;

    // execute harvest
    let harvest_msg = ExecuteMsg::Harvest {};
    let harvest_msg = to_binary(&harvest_msg)?;
    let harvest_cosmos_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: env.contract.address.to_string(),
        msg: harvest_msg,
        funds: vec![],
    });

    Ok(Response::new()
        .add_message(harvest_cosmos_msg)
        .add_attribute("action", "steakhub/submit_proof"))
}
