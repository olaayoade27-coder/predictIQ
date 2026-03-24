use crate::errors::ErrorCode;
use crate::modules::{markets, sac};
use crate::types::{Bet, MarketStatus};
use soroban_sdk::{contracttype, token, Address, Env};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Bet(u64, Address), // market_id, bettor
    MarketRemainder(u64), // Accumulated remainder for a market
    ClaimCount(u64), // Number of claims processed for a market
}

pub fn place_bet(
    e: &Env,
    bettor: Address,
    market_id: u64,
    outcome: u32,
    amount: i128,
    token_address: Address,
    referrer: Option<Address>,
) -> Result<(), ErrorCode> {
    bettor.require_auth();

    // Check if contract is paused - high-risk operation
    crate::modules::circuit_breaker::require_not_paused_for_high_risk(e)?;

    let mut market = markets::get_market(e, market_id).ok_or(ErrorCode::MarketNotFound)?;

    if market.status != MarketStatus::Active {
        return Err(ErrorCode::MarketNotActive);
    }

    // Validate parent market conditions for conditional markets
    if market.parent_id > 0 {
        let parent_market =
            markets::get_market(e, market.parent_id).ok_or(ErrorCode::MarketNotFound)?;

        // Parent must be resolved
        if parent_market.status != MarketStatus::Resolved {
            return Err(ErrorCode::ParentMarketNotResolved);
        }

        // Parent must have resolved to the required outcome
        let parent_winning_outcome = parent_market
            .winning_outcome
            .ok_or(ErrorCode::ParentMarketNotResolved)?;
        if parent_winning_outcome != market.parent_outcome_idx {
            return Err(ErrorCode::ParentMarketInvalidOutcome);
        }
    }

    if e.ledger().timestamp() >= market.deadline {
        return Err(ErrorCode::DeadlinePassed);
    }

    if outcome >= market.options.len() {
        return Err(ErrorCode::InvalidOutcome);
    }

    // Validate token_address matches market's configured asset
    if token_address != market.token_address {
        return Err(ErrorCode::InvalidBetAmount);
    }

    // Transfer tokens from bettor to contract using SAC-safe transfer
    sac::safe_transfer(
        e,
        &token_address,
        &bettor,
        &e.current_contract_address(),
        &amount,
    )?;

    let bet_key = DataKey::Bet(market_id, bettor.clone());
    let mut existing_bet: Bet = e.storage().persistent().get(&bet_key).unwrap_or(Bet {
        market_id,
        bettor: bettor.clone(),
        outcome,
        amount: 0,
    });

    if existing_bet.amount > 0 && existing_bet.outcome != outcome {
        return Err(ErrorCode::CannotChangeOutcome);
    }

    existing_bet.amount += amount;
    market.total_staked += amount;

    let outcome_stake = market.outcome_stakes.get(outcome).unwrap_or(0);
    market.outcome_stakes.set(outcome, outcome_stake + amount);

    e.storage().persistent().set(&bet_key, &existing_bet);
    markets::update_market(e, market);

    // Bump TTL for market data to prevent state expiration
    markets::bump_market_ttl(e, market_id);

    // Emit standardized BetPlaced event
    // Topics: [BetPlaced, market_id, bettor]
    crate::modules::events::emit_bet_placed(e, market_id, bettor, outcome, amount);

    Ok(())
}

pub fn get_bet(e: &Env, market_id: u64, bettor: Address) -> Option<Bet> {
    e.storage()
        .persistent()
        .get(&DataKey::Bet(market_id, bettor))
}

pub fn claim_winnings(
    e: &Env,
    bettor: Address,
    market_id: u64,
    token_address: Address,
) -> Result<i128, ErrorCode> {
    bettor.require_auth();

    let market = markets::get_market(e, market_id).ok_or(ErrorCode::MarketNotFound)?;

    if market.status != MarketStatus::Resolved {
        return Err(ErrorCode::MarketNotPendingResolution);
    }

    let winning_outcome = market
        .winning_outcome
        .ok_or(ErrorCode::MarketNotPendingResolution)?;

    let bet_key = DataKey::Bet(market_id, bettor.clone());
    let bet: Bet = e
        .storage()
        .persistent()
        .get(&bet_key)
        .ok_or(ErrorCode::MarketNotFound)?;

    if bet.outcome != winning_outcome {
        return Err(ErrorCode::InvalidOutcome);
    }

    // Get winning outcome stake
    let winning_stake = market
        .outcome_stakes
        .get(winning_outcome)
        .unwrap_or(0);

    if winning_stake == 0 {
        return Err(ErrorCode::InvalidOutcome);
    }

    // Calculate parimutuel payout
    // Formula: (bettor_stake / winning_stake) * total_pool
    // This ensures all winners share the total pool proportionally
    let base_winnings = (bet.amount * market.total_staked) / winning_stake;
    
    // Calculate and accumulate remainder from integer division
    let remainder = (bet.amount * market.total_staked) % winning_stake;
    let remainder_key = DataKey::MarketRemainder(market_id);
    let accumulated_remainder: i128 = e
        .storage()
        .persistent()
        .get(&remainder_key)
        .unwrap_or(0);
    let new_remainder = accumulated_remainder + remainder;
    
    // Track claim count to determine if this is the last claim
    let claim_count_key = DataKey::ClaimCount(market_id);
    let claim_count: u32 = e
        .storage()
        .persistent()
        .get(&claim_count_key)
        .unwrap_or(0);
    let new_claim_count = claim_count + 1;
    
    // Calculate total number of winning bets (this is an approximation)
    // In a production system, you'd track this more precisely
    let estimated_winner_count = winning_stake / bet.amount;
    
    // If this appears to be the last or near-last claim, add accumulated remainder
    let mut winnings = base_winnings;
    if new_claim_count >= estimated_winner_count.saturating_sub(1) as u32 && new_remainder > 0 {
        winnings += new_remainder;
        e.storage().persistent().set(&remainder_key, &0);
    } else {
        e.storage().persistent().set(&remainder_key, &new_remainder);
    }
    
    e.storage().persistent().set(&claim_count_key, &new_claim_count);

    // Transfer winnings to bettor
    let client = token::Client::new(e, &token_address);
    client.transfer(&e.current_contract_address(), &bettor, &winnings);

    // Remove bet record
    e.storage().persistent().remove(&bet_key);

    // Emit standardized RewardsClaimed event
    // Topics: [RewardsClaimed, market_id, bettor]
    crate::modules::events::emit_rewards_claimed(e, market_id, bettor, winnings, false);

    Ok(winnings)
}

pub fn withdraw_refund(
    e: &Env,
    bettor: Address,
    market_id: u64,
    token_address: Address,
) -> Result<i128, ErrorCode> {
    bettor.require_auth();

    let market = markets::get_market(e, market_id).ok_or(ErrorCode::MarketNotFound)?;

    if market.status != MarketStatus::Cancelled {
        return Err(ErrorCode::MarketNotActive);
    }

    let bet_key = DataKey::Bet(market_id, bettor.clone());
    let bet: Bet = e
        .storage()
        .persistent()
        .get(&bet_key)
        .ok_or(ErrorCode::MarketNotFound)?;

    let refund_amount = bet.amount;

    // Transfer refund to bettor
    let client = token::Client::new(e, &token_address);
    client.transfer(&e.current_contract_address(), &bettor, &refund_amount);

    // Remove bet record
    e.storage().persistent().remove(&bet_key);

    // Emit standardized RewardsClaimed event (refund variant)
    // Topics: [RewardsClaimed, market_id, bettor]
    crate::modules::events::emit_rewards_claimed(e, market_id, bettor, refund_amount, true);

    Ok(refund_amount)
}

/// Get the accumulated remainder for a market
pub fn get_market_remainder(e: &Env, market_id: u64) -> i128 {
    e.storage()
        .persistent()
        .get(&DataKey::MarketRemainder(market_id))
        .unwrap_or(0)
}

/// Collect unclaimed remainder to fee treasury after market is resolved
/// Can only be called by admin after a grace period
pub fn collect_market_remainder(
    e: &Env,
    market_id: u64,
    token_address: Address,
) -> Result<i128, ErrorCode> {
    crate::modules::admin::require_admin(e)?;
    
    let market = markets::get_market(e, market_id).ok_or(ErrorCode::MarketNotFound)?;
    
    // Market must be resolved
    if market.status != crate::types::MarketStatus::Resolved {
        return Err(ErrorCode::MarketNotActive);
    }
    
    // Check if sufficient time has passed (e.g., 30 days)
    let resolved_at = market.resolved_at.ok_or(ErrorCode::MarketNotActive)?;
    let grace_period = 30 * 24 * 60 * 60; // 30 days in seconds
    if e.ledger().timestamp() < resolved_at + grace_period {
        return Err(ErrorCode::MarketStillActive);
    }
    
    let remainder_key = DataKey::MarketRemainder(market_id);
    let remainder: i128 = e
        .storage()
        .persistent()
        .get(&remainder_key)
        .unwrap_or(0);
    
    if remainder == 0 {
        return Err(ErrorCode::InsufficientBalance);
    }
    
    // Transfer remainder to fee treasury
    crate::modules::fees::collect_fee(e, token_address.clone(), remainder);
    
    let client = token::Client::new(e, &token_address);
    client.transfer(&e.current_contract_address(), &crate::modules::admin::get_fee_admin(e)?, &remainder);
    
    // Clear the remainder
    e.storage().persistent().set(&remainder_key, &0);
    
    Ok(remainder)
}
