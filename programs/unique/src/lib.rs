//! lowest.one — lowest unique number wins. Paid in $UNIQUE.
//!
//! v2: one table, $5 a pick, a round every hour. (Accounts are keyed by table id, so more
//! tables are possible, but the game runs just one.) Any whole number, any amount of picks.
//! A round with no picks just waits; the first pick fixes its close at the next scheduled time
//! (every `period_secs`, shifted by `close_offset_secs`: hourly at :00), however many follow.
//!
//! Round lifecycle (phases are derived from the clock, so nobody has to "flip" them):
//!   Open      anyone enters with a sealed number, paying from their wallet or their stake.
//!             Picks can be changed until REPICK_CUTOFF_SECS before the close.
//!   Reveal    players reveal (number + salt). Each reveal bumps a per-number tally and refunds
//!             the reveal deposit. The window ends early if every entry is revealed.
//!   Claim     anyone points at a revealed entry whose number was revealed exactly once.
//!             The round keeps the MAX_WINNERS lowest.
//!   Settle    anyone settles: the lowest claims are paid by the round's tier (more picks, more
//!             winners); unpaid shares roll over into the table's next round, which opens here.
//!
//! Every ticket is split: pool / stakers / burn / team (a referrer's share comes out of the
//! team's). Staking is shared by all tables: stake and unstake any time, rewards accrue
//! continuously, rewards that arrive while nobody is staked are released over
//! `backlog_release_secs` of staked time, and stakers can pay for picks from rewards and stake.
//!
//! Commitment format (must match the site's lib/commit.ts):
//!   sha256( number as u64 little-endian || 32-byte salt || player public key )
//!
//! v6: bundles. One pick can seal up to MAX_BUNDLE numbers under one hash, with one account and
//! one reveal deposit; every number still pays its own ticket and counts as one pick. The
//! commitment is the same format with the numbers one after another (a bundle of one hashes
//! exactly like a single entry):
//!   sha256( n1 u64 LE || n2 u64 LE || ... || 32-byte salt || player public key )
//! The single-number instructions stay, for picks made before v6.

use anchor_lang::prelude::*;
use anchor_lang::system_program;
use anchor_spl::token_interface::{self, Burn, Mint, TokenAccount, TokenInterface, TransferChecked};
use solana_sha256_hasher::hashv;

declare_id!("DGGCp8MjbdjtxENbgQVK8m2MKdckxtypQK12ssqcJ7be");

const BPS: u64 = 10_000;
const NO_CLAIM: u64 = u64::MAX;
/// Fixed-point scale for the reward-per-token index.
const SCALE: u128 = 1_000_000_000_000;
/// Up to this many winners per round. Array sizes below are written out as literals
/// (4 places, 3 tier thresholds) because InitSpace can't evaluate `MAX_WINNERS - 1`.
pub const MAX_WINNERS: usize = 4;
/// Opaque backup of the sealed number, readable only by the player (see the site's lib/commit.ts).
pub const BACKUP_LEN: usize = 24;
/// Most numbers in one bundle. Bounded by what fits in one transaction (see docs/V6-BUNDLES.md).
pub const MAX_BUNDLE: usize = 10;
/// Picks can be changed until this long before the round closes.
pub const REPICK_CUTOFF_SECS: i64 = 60;
/// The keeper may adjust a table's ticket at most this often (and by at most
/// `max_price_change_bps` each time).
pub const TICKET_UPDATE_SECS: i64 = 600;
// Limits on what the admin can configure (see ConfigParams/TableParams::validate).
const MIN_POOL_BPS: u64 = 7_000;
const MAX_MARKETING_BPS: u64 = 1_000;
const MAX_PRICE_CHANGE_BPS: u64 = 5_000;
const MIN_REVEAL_SECS: i64 = 30;
const MIN_CLAIM_SECS: i64 = 20;

#[program]
pub mod unique {
    use super::*;

    /// One-time setup: global config, the pool, reward and stake vaults, and the staking pool.
    pub fn initialize(ctx: Context<Initialize>, p: ConfigParams) -> Result<()> {
        p.validate()?;
        let now = Clock::get()?.unix_timestamp;
        let cfg = &mut ctx.accounts.config;
        cfg.admin = ctx.accounts.admin.key();
        cfg.mint = ctx.accounts.mint.key();
        cfg.team_token = ctx.accounts.team_token.key();
        cfg.apply(&p);
        cfg.entries_paused = false;
        cfg.bump = ctx.bumps.config;
        cfg.vault_bump = ctx.bumps.vault;
        cfg.reserve_bump = ctx.bumps.reserve;
        cfg.stake_vault_bump = ctx.bumps.stake_vault;

        let pool = &mut ctx.accounts.stake_pool;
        pool.total_staked = 0;
        pool.acc_per_token = 0;
        pool.backlog = 0;
        pool.stream_end = now;
        pool.last_update = now;
        pool.bump = ctx.bumps.stake_pool;
        Ok(())
    }

    /// Admin updates the global rules (split, tiers, keeper, staking). The split and tiers
    /// are copied into each round when it opens, so they apply from each table's next round.
    pub fn update_config(ctx: Context<UpdateConfig>, p: ConfigParams) -> Result<()> {
        p.validate()?;
        let cfg = &mut ctx.accounts.config;
        cfg.team_token = ctx.accounts.team_token.key();
        cfg.apply(&p);
        Ok(())
    }

    /// Admin adds a table and opens its first round.
    pub fn create_table(ctx: Context<CreateTable>, id: u8, p: TableParams) -> Result<()> {
        p.validate()?;
        let now = Clock::get()?.unix_timestamp;
        let t = &mut ctx.accounts.table;
        t.id = id;
        t.apply(&p);
        t.enabled = true;
        t.current_round = 1;
        t.ticket_set_at = 0;
        t.bump = ctx.bumps.table;
        ctx.accounts.round.open(&ctx.accounts.table, &ctx.accounts.config, 1, 0, now, ctx.bumps.round);
        emit!(TableCreated { table: id });
        Ok(())
    }

    /// Admin changes a table's rules. They apply from its next round.
    pub fn update_table(ctx: Context<UpdateTable>, p: TableParams) -> Result<()> {
        p.validate()?;
        ctx.accounts.table.apply(&p);
        Ok(())
    }

    /// Admin switches a table on or off (off = no new picks, hidden on the site). A table can
    /// only be switched off while its current round has no picks, so nobody's picks get stuck.
    pub fn set_table_enabled(ctx: Context<SetTableEnabled>, enabled: bool) -> Result<()> {
        require!(enabled || ctx.accounts.round.entries == 0, GameError::TableNotEmpty);
        ctx.accounts.table.enabled = enabled;
        emit!(TableEnabled { table: ctx.accounts.table.id, enabled });
        Ok(())
    }

    /// The keeper (or admin) keeps a table's ticket near its dollar price from the price feed:
    /// at most every `TICKET_UPDATE_SECS`, by at most `max_price_change_bps`. It applies to the
    /// table's current round too while that round is still open (each pick pays the ticket of
    /// the moment it's made; a pick's `max_ticket` makes sure nobody pays more than they saw),
    /// and to every round after.
    pub fn set_ticket(ctx: Context<SetTicket>, ticket_tokens: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let cfg = &ctx.accounts.config;
        let who = ctx.accounts.signer.key();
        require!(who == cfg.keeper || who == cfg.admin, GameError::Unauthorized);
        let t = &mut ctx.accounts.table;
        require!(now - t.ticket_set_at >= TICKET_UPDATE_SECS, GameError::PriceAlreadySet);
        let old = t.ticket_tokens;
        require!(ticket_tokens > 0 && ticket_tokens.abs_diff(old) <= bps(old, cfg.max_price_change_bps), GameError::PriceChangeTooBig);
        t.ticket_tokens = ticket_tokens;
        t.ticket_set_at = now;
        let r = &mut ctx.accounts.round;
        if r.phase(now) == Phase::Open { r.ticket_tokens = ticket_tokens; }
        emit!(TicketSet { table: t.id, round: t.current_round, ticket_tokens });
        Ok(())
    }

    /// Give an open round that has picks but no close time yet its close: the next scheduled
    /// time. Only rounds opened before v2 can be in that state (they waited for a minimum
    /// number of picks; now the first pick sets the close). Anyone can call this.
    pub fn start_close(ctx: Context<StartClose>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let r = &mut ctx.accounts.round;
        require!(r.phase(now) == Phase::Open && r.close_at == 0 && r.entries > 0, GameError::NothingToStart);
        schedule_close(r, now);
        Ok(())
    }

    /// Admin stops or resumes new picks on every table. Nothing else is affected: reveals,
    /// claims, payouts, staking, unstaking and reward claims keep working.
    pub fn set_paused(ctx: Context<AdminOnly>, paused: bool) -> Result<()> {
        ctx.accounts.config.entries_paused = paused;
        emit!(EntriesPaused { paused });
        Ok(())
    }

    /// Pay the ticket from your wallet and seal a number. The number itself never goes on-chain
    /// here; `backup` is the player's own encrypted copy so they can reveal from any device.
    /// `max_ticket`: the most the player agrees to pay (the price the site showed them). The
    /// keeper can adjust an open round's ticket, so a pick sent just before a rise is refused
    /// rather than charged more.
    pub fn enter(ctx: Context<Enter>, commitment: [u8; 32], backup: [u8; BACKUP_LEN], max_ticket: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(!ctx.accounts.config.entries_paused, GameError::EntriesPaused);
        require!(ctx.accounts.table.enabled, GameError::TableDisabled);
        require!(ctx.accounts.round.phase(now) == Phase::Open, GameError::EntriesClosed);
        let referrer = bind_referrer(&ctx.accounts.profile.to_account_info(), &ctx.accounts.player.to_account_info(), &ctx.accounts.system_program, ctx.bumps.profile, &ctx.accounts.referrer_token)?;
        let split = Split::of(&ctx.accounts.round, referrer.is_some());
        require!(split.ticket <= max_ticket, GameError::TicketPriceChanged);

        let a = &ctx.accounts;
        let player = a.player.to_account_info();
        let from = a.player_token.to_account_info();
        pay(&a.token_program, &a.mint, from.clone(), a.vault.to_account_info(), player.clone(), split.pool, &[])?;
        pay(&a.token_program, &a.mint, from.clone(), a.reserve.to_account_info(), player.clone(), split.rewards, &[])?;
        pay(&a.token_program, &a.mint, from.clone(), a.team_token.to_account_info(), player.clone(), split.team, &[])?;
        if let Some(r) = &a.referrer_token {
            pay(&a.token_program, &a.mint, from.clone(), r.to_account_info(), player.clone(), split.referral, &[])?;
        }
        burn(&a.token_program, &a.mint, from, player.clone(), split.burn, &[])?;
        take_deposit(&a.system_program, player, a.entry.to_account_info(), split.deposit)?;
        if let Some(owner) = referrer {
            emit!(Referred { table: a.round.table_id, round: a.round.id, player: a.player.key(), referrer: owner, amount: split.referral });
        }

        let release = ctx.accounts.config.backlog_release_secs;
        let sp = &mut ctx.accounts.stake_pool;
        sp.update(now)?;
        sp.add_rewards(split.rewards, now, release)?;

        let (key, round_key) = (ctx.accounts.player.key(), ctx.accounts.round.key());
        record_entry(&mut ctx.accounts.round, round_key, &mut ctx.accounts.entry, key, commitment, backup, &split, now, ctx.bumps.entry)
    }

    /// Same as `enter`, but the ticket is paid from the player's unclaimed staking rewards first
    /// and then their stake. If what's left staked would be under the minimum, it's returned
    /// to their wallet.
    /// `max_ticket` as in `enter`.
    pub fn enter_from_stake(ctx: Context<EnterFromStake>, commitment: [u8; 32], backup: [u8; BACKUP_LEN], max_ticket: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(!ctx.accounts.config.entries_paused, GameError::EntriesPaused);
        require!(ctx.accounts.table.enabled, GameError::TableDisabled);
        require!(ctx.accounts.round.phase(now) == Phase::Open, GameError::EntriesClosed);
        let referrer = bind_referrer(&ctx.accounts.profile.to_account_info(), &ctx.accounts.player.to_account_info(), &ctx.accounts.system_program, ctx.bumps.profile, &ctx.accounts.referrer_token)?;
        let split = Split::of(&ctx.accounts.round, referrer.is_some());
        require!(split.ticket <= max_ticket, GameError::TicketPriceChanged);
        let min_stake = ctx.accounts.config.min_stake_tokens;
        let release = ctx.accounts.config.backlog_release_secs;

        // Work out where the ticket comes from and update the books first.
        let (from_rewards, from_stake, dust) = {
            let sp = &mut ctx.accounts.stake_pool;
            sp.update(now)?;
            let st = &mut ctx.accounts.staker;
            st.sync(sp.acc_per_token)?;
            let from_rewards = st.pending.min(split.ticket);
            let from_stake = split.ticket - from_rewards;
            require!(from_stake <= st.amount, GameError::NotEnoughStaked);
            st.pending -= from_rewards;
            st.amount -= from_stake;
            let dust = if st.amount > 0 && st.amount < min_stake { st.amount } else { 0 };
            st.amount -= dust;
            sp.total_staked -= from_stake + dust;
            st.debt = st.owed(sp.acc_per_token)?;
            (from_rewards, from_stake, dust)
        };

        let a = &ctx.accounts;
        let bump = [a.config.bump];
        let seeds: &[&[&[u8]]] = &[&[b"config", &bump]];
        let cfg = a.config.to_account_info();
        let vault = a.vault.to_account_info();
        // Gather the ticket in the pool vault, then split it from there.
        pay(&a.token_program, &a.mint, a.reserve.to_account_info(), vault.clone(), cfg.clone(), from_rewards, seeds)?;
        pay(&a.token_program, &a.mint, a.stake_vault.to_account_info(), vault.clone(), cfg.clone(), from_stake, seeds)?;
        pay(&a.token_program, &a.mint, a.stake_vault.to_account_info(), a.player_token.to_account_info(), cfg.clone(), dust, seeds)?;
        pay(&a.token_program, &a.mint, vault.clone(), a.reserve.to_account_info(), cfg.clone(), split.rewards, seeds)?;
        pay(&a.token_program, &a.mint, vault.clone(), a.team_token.to_account_info(), cfg.clone(), split.team, seeds)?;
        if let Some(r) = &a.referrer_token {
            pay(&a.token_program, &a.mint, vault.clone(), r.to_account_info(), cfg.clone(), split.referral, seeds)?;
        }
        burn(&a.token_program, &a.mint, vault, cfg, split.burn, seeds)?;
        take_deposit(&a.system_program, a.player.to_account_info(), a.entry.to_account_info(), split.deposit)?;

        ctx.accounts.stake_pool.add_rewards(split.rewards, now, release)?;
        let player = ctx.accounts.player.key();
        emit!(PaidFromStake { player, from_rewards, from_stake, returned: dust });
        if let Some(owner) = referrer {
            emit!(Referred { table: ctx.accounts.round.table_id, round: ctx.accounts.round.id, player, referrer: owner, amount: split.referral });
        }
        let round_key = ctx.accounts.round.key();
        record_entry(&mut ctx.accounts.round, round_key, &mut ctx.accounts.entry, player, commitment, backup, &split, now, ctx.bumps.entry)
    }

    /// Change your mind: swap a sealed pick for a new one, until REPICK_CUTOFF_SECS before the
    /// round closes. The old entry is closed (its rent and deposit come back) and a new one takes
    /// its place; the ticket already paid carries over.
    pub fn repick(ctx: Context<Repick>, new_commitment: [u8; 32], new_backup: [u8; BACKUP_LEN]) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let r = &ctx.accounts.round;
        require!(r.phase(now) == Phase::Open, GameError::EntriesClosed);
        require!(r.close_at == 0 || now < r.close_at - REPICK_CUTOFF_SECS, GameError::RepickClosed);
        let old = &ctx.accounts.old_entry;
        let (deposit, round, player) = (old.deposit, old.round, old.player);
        let old_commitment = old.commitment;
        take_deposit(&ctx.accounts.system_program, ctx.accounts.player.to_account_info(), ctx.accounts.new_entry.to_account_info(), deposit)?;
        let e = &mut ctx.accounts.new_entry;
        e.round = round;
        e.player = player;
        e.commitment = new_commitment;
        e.backup = new_backup;
        e.number = 0;
        e.revealed = false;
        e.deposit = deposit;
        e.bump = ctx.bumps.new_entry;
        emit!(Repicked { table: r.table_id, round: r.id, player, old_commitment, new_commitment });
        Ok(())
    }

    /// Open a sealed entry. Anyone may send this (the site or keeper does it for the player),
    /// but only the right number + salt for this player matches the commitment.
    pub fn reveal(ctx: Context<Reveal>, number: u64, salt: [u8; 32]) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let r = &ctx.accounts.round;
        require!(r.phase(now) == Phase::Reveal, GameError::NotRevealTime);
        require!(number >= 1 && number != NO_CLAIM, GameError::BadNumber);

        let e = &mut ctx.accounts.entry;
        require!(!e.revealed, GameError::AlreadyRevealed);
        let h = hashv(&[number.to_le_bytes().as_ref(), salt.as_ref(), e.player.as_ref()]);
        require!(h.to_bytes() == e.commitment, GameError::CommitmentMismatch);
        e.revealed = true;
        e.number = number;

        let t = &mut ctx.accounts.tally;
        if t.count == 0 {
            t.round = ctx.accounts.round.key();
            t.number = number;
            t.payer = ctx.accounts.payer.key();
            t.bump = ctx.bumps.tally;
        }
        t.count = t.count.saturating_add(1);

        let r = &mut ctx.accounts.round;
        r.revealed += 1;
        // Everyone's revealed: no need to wait out the window, the claim starts now.
        if r.revealed == r.entries {
            r.all_revealed_at = now;
        }

        let d = ctx.accounts.entry.deposit;
        ctx.accounts.entry.deposit = 0;
        ctx.accounts.entry.sub_lamports(d)?;
        ctx.accounts.player.add_lamports(d)?;
        emit!(Revealed { table: r.table_id, round: r.id, player: ctx.accounts.entry.player, number });
        Ok(())
    }

    /// Point at a revealed entry whose number nobody else revealed. The round keeps the
    /// MAX_WINNERS lowest such numbers, in order. Anyone can call this.
    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(ctx.accounts.round.phase(now) == Phase::Claim, GameError::NotClaimTime);
        let e = &ctx.accounts.entry;
        require!(e.revealed, GameError::NotRevealed);
        require!(ctx.accounts.tally.count == 1, GameError::NotUnique);

        let (n, player) = (e.number, e.player);
        claim_number(&mut ctx.accounts.round, n, player)
    }

    /// Pay the winners (or roll the pool over) and open the table's next round. Anyone can call
    /// this. Pass the winners' token accounts as remaining accounts, lowest number first, one
    /// for each place that gets paid.
    pub fn settle<'info>(ctx: Context<'info, Settle<'info>>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(ctx.accounts.round.phase(now) == Phase::Settleable, GameError::TooEarly);

        let r = &ctx.accounts.round;
        let pool = r.pool_tokens;
        let places = r.places();
        let claimed = r.claims.iter().filter(|c| c.number != NO_CLAIM).count();
        let paid_places = places.min(claimed);
        let split = r.tier_splits[places - 1];
        let claims = r.claims;
        require!(ctx.remaining_accounts.len() >= paid_places, GameError::WinnerAccountMissing);

        let a = &ctx.accounts;
        let bump = [a.config.bump];
        let seeds: &[&[&[u8]]] = &[&[b"config", &bump]];
        let mut paid_total = 0u64;
        let mut paid: Vec<WinnerPaid> = Vec::with_capacity(paid_places);
        for (place, acc) in ctx.remaining_accounts.iter().take(paid_places).enumerate() {
            require_keys_eq!(*acc.owner, a.token_program.key(), GameError::WrongWinnerAccount);
            let ta = TokenAccount::try_deserialize(&mut &acc.try_borrow_data()?[..])?;
            require_keys_eq!(ta.owner, claims[place].player, GameError::WrongWinnerAccount);
            require_keys_eq!(ta.mint, a.mint.key(), GameError::WrongWinnerAccount);
            let amount = bps(pool, split[place]);
            pay(&a.token_program, &a.mint, a.vault.to_account_info(), acc.clone(), a.config.to_account_info(), amount, seeds)?;
            paid_total += amount;
            paid.push(WinnerPaid { player: claims[place].player, number: claims[place].number, amount });
        }
        // Shares for places nobody claimed, plus rounding dust, roll over.
        let carry = pool - paid_total;

        let r = &mut ctx.accounts.round;
        r.settled = true;
        emit!(Settled { table: r.table_id, round: r.id, pool_tokens: pool, winners: paid, rolled_over: carry });

        let next_id = r.id + 1;
        ctx.accounts.next_round.open(&ctx.accounts.table, &ctx.accounts.config, next_id, carry, now, ctx.bumps.next_round);
        ctx.accounts.table.current_round = next_id;
        Ok(())
    }

    /// Anyone can top up a table's current pool (e.g. a treasury seed).
    pub fn fund_pool(ctx: Context<FundPool>, amount: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(ctx.accounts.round.phase(now) == Phase::Open, GameError::EntriesClosed);
        let a = &ctx.accounts;
        pay(&a.token_program, &a.mint, a.funder_token.to_account_info(), a.vault.to_account_info(), a.funder.to_account_info(), amount, &[])?;
        let r = &mut ctx.accounts.round;
        r.pool_tokens = r.pool_tokens.checked_add(amount).ok_or(GameError::Overflow)?;
        Ok(())
    }

    /// Stake $UNIQUE. Rewards start accruing immediately.
    pub fn stake(ctx: Context<Stake>, amount: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(amount > 0, GameError::BadAmount);
        let min_stake = ctx.accounts.config.min_stake_tokens;
        {
            let sp = &mut ctx.accounts.stake_pool;
            sp.update(now)?;
            let st = &mut ctx.accounts.staker;
            if st.owner == Pubkey::default() {
                st.owner = ctx.accounts.owner.key();
                st.bump = ctx.bumps.staker;
            }
            st.sync(sp.acc_per_token)?;
            st.amount = st.amount.checked_add(amount).ok_or(GameError::Overflow)?;
            require!(st.amount >= min_stake, GameError::BelowMinimumStake);
            sp.total_staked = sp.total_staked.checked_add(amount).ok_or(GameError::Overflow)?;
            st.debt = st.owed(sp.acc_per_token)?;
        }
        let a = &ctx.accounts;
        pay(&a.token_program, &a.mint, a.owner_token.to_account_info(), a.stake_vault.to_account_info(), a.owner.to_account_info(), amount, &[])?;
        emit!(Staked { owner: a.owner.key(), amount, total: a.staker.amount });
        Ok(())
    }

    /// Unstake any amount, instantly. What's left must be zero or at least the minimum stake.
    /// Unclaimed rewards stay claimable.
    pub fn unstake(ctx: Context<Unstake>, amount: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(amount > 0, GameError::BadAmount);
        let min_stake = ctx.accounts.config.min_stake_tokens;
        {
            let sp = &mut ctx.accounts.stake_pool;
            sp.update(now)?;
            let st = &mut ctx.accounts.staker;
            st.sync(sp.acc_per_token)?;
            require!(amount <= st.amount, GameError::NotEnoughStaked);
            st.amount -= amount;
            require!(st.amount == 0 || st.amount >= min_stake, GameError::BelowMinimumStake);
            sp.total_staked -= amount;
            st.debt = st.owed(sp.acc_per_token)?;
        }
        let a = &ctx.accounts;
        let bump = [a.config.bump];
        let seeds: &[&[&[u8]]] = &[&[b"config", &bump]];
        pay(&a.token_program, &a.mint, a.stake_vault.to_account_info(), a.owner_token.to_account_info(), a.config.to_account_info(), amount, seeds)?;
        emit!(Unstaked { owner: a.owner.key(), amount, total: a.staker.amount });
        Ok(())
    }

    /// Claim all accrued staking rewards to your wallet.
    pub fn claim_rewards(ctx: Context<ClaimRewards>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let amount = {
            let sp = &mut ctx.accounts.stake_pool;
            sp.update(now)?;
            let st = &mut ctx.accounts.staker;
            st.sync(sp.acc_per_token)?;
            let p = st.pending;
            require!(p > 0, GameError::NothingToClaim);
            st.pending = 0;
            p
        };
        let a = &ctx.accounts;
        let bump = [a.config.bump];
        let seeds: &[&[&[u8]]] = &[&[b"config", &bump]];
        pay(&a.token_program, &a.mint, a.reserve.to_account_info(), a.owner_token.to_account_info(), a.config.to_account_info(), amount, seeds)?;
        emit!(RewardsClaimed { owner: a.owner.key(), amount });
        Ok(())
    }

    /// After settlement, close an entry to reclaim its rent. Revealed entries refund the
    /// player; unrevealed ones forfeit their deposit (and rent) to the team wallet.
    pub fn close_entry(ctx: Context<CloseEntry>) -> Result<()> {
        require!(ctx.accounts.round.settled, GameError::TooEarly);
        let dest = if ctx.accounts.entry.revealed {
            ctx.accounts.player.to_account_info()
        } else {
            ctx.accounts.team_wallet.to_account_info()
        };
        ctx.accounts.entry.close(dest)
    }

    /// After settlement, close a number tally and refund its rent to whoever paid for it.
    pub fn close_tally(ctx: Context<CloseTally>) -> Result<()> {
        require!(ctx.accounts.round.settled, GameError::TooEarly);
        Ok(())
    }

    // --- v6: bundles ---------------------------------------------------------------

    /// Like `enter`, for a bundle of `sealed.len()` numbers (1 to MAX_BUNDLE): pays that many
    /// tickets and one reveal deposit. `nonce` and `sealed` are the player's own encrypted copy
    /// (see the site's lib/commit.ts); `sealed` is overwritten with the numbers at reveal.
    pub fn enter_bundle(ctx: Context<EnterBundle>, commitment: [u8; 32], nonce: [u8; 16], sealed: Vec<u64>, max_ticket: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let k = bundle_len(sealed.len())?;
        require!(!ctx.accounts.config.entries_paused, GameError::EntriesPaused);
        require!(ctx.accounts.table.enabled, GameError::TableDisabled);
        require!(ctx.accounts.round.phase(now) == Phase::Open, GameError::EntriesClosed);
        let referrer = bind_referrer(&ctx.accounts.profile.to_account_info(), &ctx.accounts.player.to_account_info(), &ctx.accounts.system_program, ctx.bumps.profile, &ctx.accounts.referrer_token)?;
        let one = Split::of(&ctx.accounts.round, referrer.is_some());
        require!(one.ticket <= max_ticket, GameError::TicketPriceChanged);
        let split = one.times(k)?;

        let a = &ctx.accounts;
        let player = a.player.to_account_info();
        let from = a.player_token.to_account_info();
        pay(&a.token_program, &a.mint, from.clone(), a.vault.to_account_info(), player.clone(), split.pool, &[])?;
        pay(&a.token_program, &a.mint, from.clone(), a.reserve.to_account_info(), player.clone(), split.rewards, &[])?;
        pay(&a.token_program, &a.mint, from.clone(), a.team_token.to_account_info(), player.clone(), split.team, &[])?;
        if let Some(r) = &a.referrer_token {
            pay(&a.token_program, &a.mint, from.clone(), r.to_account_info(), player.clone(), split.referral, &[])?;
        }
        burn(&a.token_program, &a.mint, from, player.clone(), split.burn, &[])?;
        take_deposit(&a.system_program, player, a.bundle.to_account_info(), split.deposit)?;
        if let Some(owner) = referrer {
            emit!(Referred { table: a.round.table_id, round: a.round.id, player: a.player.key(), referrer: owner, amount: split.referral });
        }

        let release = ctx.accounts.config.backlog_release_secs;
        let sp = &mut ctx.accounts.stake_pool;
        sp.update(now)?;
        sp.add_rewards(split.rewards, now, release)?;

        let (key, round_key) = (ctx.accounts.player.key(), ctx.accounts.round.key());
        record_bundle(&mut ctx.accounts.round, round_key, &mut ctx.accounts.bundle, key, commitment, nonce, sealed, &split, now, ctx.bumps.bundle)
    }

    /// Like `enter_from_stake`, for a bundle (as `enter_bundle`).
    pub fn enter_bundle_from_stake(ctx: Context<EnterBundleFromStake>, commitment: [u8; 32], nonce: [u8; 16], sealed: Vec<u64>, max_ticket: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let k = bundle_len(sealed.len())?;
        require!(!ctx.accounts.config.entries_paused, GameError::EntriesPaused);
        require!(ctx.accounts.table.enabled, GameError::TableDisabled);
        require!(ctx.accounts.round.phase(now) == Phase::Open, GameError::EntriesClosed);
        let referrer = bind_referrer(&ctx.accounts.profile.to_account_info(), &ctx.accounts.player.to_account_info(), &ctx.accounts.system_program, ctx.bumps.profile, &ctx.accounts.referrer_token)?;
        let one = Split::of(&ctx.accounts.round, referrer.is_some());
        require!(one.ticket <= max_ticket, GameError::TicketPriceChanged);
        let split = one.times(k)?;
        let min_stake = ctx.accounts.config.min_stake_tokens;
        let release = ctx.accounts.config.backlog_release_secs;

        // Work out where the tickets come from and update the books first.
        let (from_rewards, from_stake, dust) = {
            let sp = &mut ctx.accounts.stake_pool;
            sp.update(now)?;
            let st = &mut ctx.accounts.staker;
            st.sync(sp.acc_per_token)?;
            let from_rewards = st.pending.min(split.ticket);
            let from_stake = split.ticket - from_rewards;
            require!(from_stake <= st.amount, GameError::NotEnoughStaked);
            st.pending -= from_rewards;
            st.amount -= from_stake;
            let dust = if st.amount > 0 && st.amount < min_stake { st.amount } else { 0 };
            st.amount -= dust;
            sp.total_staked -= from_stake + dust;
            st.debt = st.owed(sp.acc_per_token)?;
            (from_rewards, from_stake, dust)
        };

        let a = &ctx.accounts;
        let bump = [a.config.bump];
        let seeds: &[&[&[u8]]] = &[&[b"config", &bump]];
        let cfg = a.config.to_account_info();
        let vault = a.vault.to_account_info();
        // Gather the tickets in the pool vault, then split them from there.
        pay(&a.token_program, &a.mint, a.reserve.to_account_info(), vault.clone(), cfg.clone(), from_rewards, seeds)?;
        pay(&a.token_program, &a.mint, a.stake_vault.to_account_info(), vault.clone(), cfg.clone(), from_stake, seeds)?;
        pay(&a.token_program, &a.mint, a.stake_vault.to_account_info(), a.player_token.to_account_info(), cfg.clone(), dust, seeds)?;
        pay(&a.token_program, &a.mint, vault.clone(), a.reserve.to_account_info(), cfg.clone(), split.rewards, seeds)?;
        pay(&a.token_program, &a.mint, vault.clone(), a.team_token.to_account_info(), cfg.clone(), split.team, seeds)?;
        if let Some(r) = &a.referrer_token {
            pay(&a.token_program, &a.mint, vault.clone(), r.to_account_info(), cfg.clone(), split.referral, seeds)?;
        }
        burn(&a.token_program, &a.mint, vault, cfg, split.burn, seeds)?;
        take_deposit(&a.system_program, a.player.to_account_info(), a.bundle.to_account_info(), split.deposit)?;

        ctx.accounts.stake_pool.add_rewards(split.rewards, now, release)?;
        let player = ctx.accounts.player.key();
        emit!(PaidFromStake { player, from_rewards, from_stake, returned: dust });
        if let Some(owner) = referrer {
            emit!(Referred { table: ctx.accounts.round.table_id, round: ctx.accounts.round.id, player, referrer: owner, amount: split.referral });
        }
        let round_key = ctx.accounts.round.key();
        record_bundle(&mut ctx.accounts.round, round_key, &mut ctx.accounts.bundle, player, commitment, nonce, sealed, &split, now, ctx.bumps.bundle)
    }

    /// Like `repick`, for a bundle: swap it for a new bundle of the same size (changing one
    /// number re-seals the whole bundle). The tickets carry over; the old bundle's rent and
    /// deposit come back.
    pub fn repick_bundle(ctx: Context<RepickBundle>, new_commitment: [u8; 32], nonce: [u8; 16], sealed: Vec<u64>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let r = &ctx.accounts.round;
        require!(r.phase(now) == Phase::Open, GameError::EntriesClosed);
        require!(r.close_at == 0 || now < r.close_at - REPICK_CUTOFF_SECS, GameError::RepickClosed);
        let old = &ctx.accounts.old_bundle;
        require!(sealed.len() == old.numbers.len(), GameError::BundleSizeChanged);
        let (deposit, round, player, old_commitment) = (old.deposit, old.round, old.player, old.commitment);
        take_deposit(&ctx.accounts.system_program, ctx.accounts.player.to_account_info(), ctx.accounts.new_bundle.to_account_info(), deposit)?;
        let b = &mut ctx.accounts.new_bundle;
        b.round = round;
        b.player = player;
        b.commitment = new_commitment;
        b.nonce = nonce;
        b.revealed = false;
        b.deposit = deposit;
        b.bump = ctx.bumps.new_bundle;
        b.numbers = sealed;
        emit!(Repicked { table: r.table_id, round: r.id, player, old_commitment, new_commitment });
        Ok(())
    }

    /// Open a bundle: all its numbers, in order, and the salt. Anyone may send this. Pass each
    /// number's tally account (["num", round, number]) as remaining accounts, in the same order
    /// (a number twice in the bundle: its tally twice); new ones are created, paid by `payer`.
    pub fn reveal_bundle<'info>(ctx: Context<'info, RevealBundle<'info>>, numbers: Vec<u64>, salt: [u8; 32]) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(ctx.accounts.round.phase(now) == Phase::Reveal, GameError::NotRevealTime);
        let b = &ctx.accounts.bundle;
        require!(!b.revealed, GameError::AlreadyRevealed);
        require!(numbers.len() == b.numbers.len(), GameError::BadBundle);
        require!(numbers.iter().all(|&n| n >= 1 && n != NO_CLAIM), GameError::BadNumber);
        let packed: Vec<u8> = numbers.iter().flat_map(|n| n.to_le_bytes()).collect();
        let h = hashv(&[packed.as_ref(), salt.as_ref(), b.player.as_ref()]);
        require!(h.to_bytes() == b.commitment, GameError::CommitmentMismatch);
        require!(ctx.remaining_accounts.len() >= numbers.len(), GameError::WrongTally);

        let round_key = ctx.accounts.round.key();
        let payer = ctx.accounts.payer.to_account_info();
        for (&n, acc) in numbers.iter().zip(ctx.remaining_accounts.iter()) {
            bump_tally(acc, round_key, n, &payer, &ctx.accounts.system_program)?;
        }

        let r = &mut ctx.accounts.round;
        r.revealed += numbers.len() as u64;
        // Everyone's revealed: no need to wait out the window, the claim starts now.
        if r.revealed == r.entries {
            r.all_revealed_at = now;
        }
        let (table, id) = (r.table_id, r.id);

        let b = &mut ctx.accounts.bundle;
        let player = b.player;
        for &number in numbers.iter() {
            emit!(Revealed { table, round: id, player, number });
        }
        b.revealed = true;
        b.numbers = numbers;
        let d = b.deposit;
        b.deposit = 0;
        ctx.accounts.bundle.sub_lamports(d)?;
        ctx.accounts.player.add_lamports(d)?;
        Ok(())
    }

    /// Like `claim`, for number `index` of a revealed bundle. Anyone can call this.
    pub fn claim_bundle(ctx: Context<ClaimBundle>, index: u8) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(ctx.accounts.round.phase(now) == Phase::Claim, GameError::NotClaimTime);
        let b = &ctx.accounts.bundle;
        require!(b.revealed, GameError::NotRevealed);
        let n = *b.numbers.get(index as usize).ok_or(GameError::BadBundle)?;
        let t = &ctx.accounts.tally;
        require!(t.round == ctx.accounts.round.key() && t.number == n, GameError::WrongTally);
        require!(t.count == 1, GameError::NotUnique);
        let player = b.player;
        claim_number(&mut ctx.accounts.round, n, player)
    }

    /// Like `close_entry`, for a bundle: revealed refunds the player; never revealed forfeits
    /// deposit and rent to the team wallet.
    pub fn close_bundle(ctx: Context<CloseBundle>) -> Result<()> {
        require!(ctx.accounts.round.settled, GameError::TooEarly);
        let dest = if ctx.accounts.bundle.revealed {
            ctx.accounts.player.to_account_info()
        } else {
            ctx.accounts.team_wallet.to_account_info()
        };
        ctx.accounts.bundle.close(dest)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn bps(amount: u64, bps: u16) -> u64 {
    (amount as u128 * bps as u128 / BPS as u128) as u64
}

/// The first scheduled close strictly after `t`: times of the form `offset + k * period`.
fn next_close(t: i64, period: i64, offset: i64) -> i64 {
    offset + period * ((t - offset).div_euclid(period) + 1)
}

/// How one ticket is split. A referrer's share comes out of the team's.
struct Split { ticket: u64, pool: u64, rewards: u64, burn: u64, referral: u64, team: u64, deposit: u64 }

impl Split {
    fn of(r: &Round, referred: bool) -> Split {
        let ticket = r.ticket_tokens;
        let pool = bps(ticket, r.pool_bps);
        let rewards = bps(ticket, r.reserve_bps);
        let burn = bps(ticket, r.burn_bps);
        let referral = if referred { bps(ticket, r.referral_bps) } else { 0 };
        Split { ticket, pool, rewards, burn, referral, team: ticket - pool - rewards - burn - referral, deposit: r.deposit_lamports }
    }

    /// `k` tickets, each split exactly as one; still one deposit (a bundle holds one).
    fn times(&self, k: usize) -> Result<Split> {
        let m = |x: u64| x.checked_mul(k as u64).ok_or(error!(GameError::Overflow));
        Ok(Split { ticket: m(self.ticket)?, pool: m(self.pool)?, rewards: m(self.rewards)?, burn: m(self.burn)?,
            referral: m(self.referral)?, team: m(self.team)?, deposit: self.deposit })
    }
}

fn bundle_len(k: usize) -> Result<usize> {
    require!((1..=MAX_BUNDLE).contains(&k), GameError::BadBundle);
    Ok(k)
}

/// Referrals stick to the wallet, not the browser. A player's first referred pick records the
/// referrer in their profile (["player", wallet]) for good; every later pick can only pay that
/// same referrer, from any device. Without a referrer token nobody is paid (the team keeps the
/// share), and wallets that never came through a link never get a profile, so they pay no rent.
/// Returns the referrer to pay, whose token account is `token` (mint checked by the account
/// constraint).
fn bind_referrer<'info>(
    profile: &AccountInfo<'info>,
    player: &AccountInfo<'info>,
    system: &Program<'info, System>,
    bump: u8,
    token: &Option<Box<InterfaceAccount<'info, TokenAccount>>>,
) -> Result<Option<Pubkey>> {
    let Some(t) = token else { return Ok(None) };
    require_keys_neq!(t.owner, player.key(), GameError::SelfReferral);
    if profile.owner == &crate::ID && !profile.data_is_empty() {
        let p = PlayerProfile::try_deserialize(&mut &profile.try_borrow_data()?[..])?;
        require_keys_eq!(p.referrer, t.owner, GameError::WrongReferrer);
        return Ok(Some(t.owner));
    }
    // First referred pick: create the profile. Transfer + allocate + assign (rather than
    // create_account) still works if someone sent lamports to the address beforehand.
    let space = 8 + PlayerProfile::INIT_SPACE;
    let rent = Rent::get()?.minimum_balance(space);
    let have = profile.lamports();
    if have < rent {
        system_program::transfer(CpiContext::new(system.key(), system_program::Transfer { from: player.clone(), to: profile.clone() }), rent - have)?;
    }
    let key = player.key();
    let bump = [bump];
    let seeds: &[&[&[u8]]] = &[&[b"player", key.as_ref(), &bump]];
    system_program::allocate(CpiContext::new_with_signer(system.key(), system_program::Allocate { account_to_allocate: profile.clone() }, seeds), space as u64)?;
    system_program::assign(CpiContext::new_with_signer(system.key(), system_program::Assign { account_to_assign: profile.clone() }, seeds), &crate::ID)?;
    PlayerProfile { referrer: t.owner, bump: bump[0] }.try_serialize(&mut &mut profile.try_borrow_mut_data()?[..])?;
    emit!(ReferrerBound { player: key, referrer: t.owner });
    Ok(Some(t.owner))
}

/// Token transfer through whichever token program the mint uses. Skips zero amounts.
fn pay<'info>(
    token_program: &Interface<'info, TokenInterface>,
    mint: &InterfaceAccount<'info, Mint>,
    from: AccountInfo<'info>,
    to: AccountInfo<'info>,
    authority: AccountInfo<'info>,
    amount: u64,
    signer: &[&[&[u8]]],
) -> Result<()> {
    if amount == 0 { return Ok(()); }
    token_interface::transfer_checked(
        CpiContext::new_with_signer(token_program.key(),
            TransferChecked { from, mint: mint.to_account_info(), to, authority }, signer),
        amount, mint.decimals)
}

fn burn<'info>(
    token_program: &Interface<'info, TokenInterface>,
    mint: &InterfaceAccount<'info, Mint>,
    from: AccountInfo<'info>,
    authority: AccountInfo<'info>,
    amount: u64,
    signer: &[&[&[u8]]],
) -> Result<()> {
    if amount == 0 { return Ok(()); }
    token_interface::burn(
        CpiContext::new_with_signer(token_program.key(), Burn { mint: mint.to_account_info(), from, authority }, signer),
        amount)
}

/// The reveal deposit sits in the entry account until the player reveals.
fn take_deposit<'info>(system: &Program<'info, System>, from: AccountInfo<'info>, entry: AccountInfo<'info>, lamports: u64) -> Result<()> {
    system_program::transfer(CpiContext::new(system.key(), system_program::Transfer { from, to: entry }), lamports)
}

#[allow(clippy::too_many_arguments)]
fn record_entry(r: &mut Round, round_key: Pubkey, e: &mut Entry, player: Pubkey, commitment: [u8; 32], backup: [u8; BACKUP_LEN],
    split: &Split, now: i64, bump: u8) -> Result<()> {
    e.round = round_key;
    e.player = player;
    e.commitment = commitment;
    e.backup = backup;
    e.number = 0;
    e.revealed = false;
    e.deposit = split.deposit;
    e.bump = bump;

    r.entries += 1;
    r.pool_tokens = r.pool_tokens.checked_add(split.pool).ok_or(GameError::Overflow)?;
    emit!(Entered { table: r.table_id, round: r.id, player, commitment, pool_tokens: r.pool_tokens });
    // The first pick fixes the close.
    if r.close_at == 0 { schedule_close(r, now); }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_bundle(r: &mut Round, round_key: Pubkey, b: &mut Bundle, player: Pubkey, commitment: [u8; 32], nonce: [u8; 16],
    sealed: Vec<u64>, split: &Split, now: i64, bump: u8) -> Result<()> {
    let count = sealed.len() as u8;
    b.round = round_key;
    b.player = player;
    b.commitment = commitment;
    b.nonce = nonce;
    b.revealed = false;
    b.deposit = split.deposit;
    b.bump = bump;
    b.numbers = sealed;

    r.entries += count as u64;
    r.pool_tokens = r.pool_tokens.checked_add(split.pool).ok_or(GameError::Overflow)?;
    emit!(BundleEntered { table: r.table_id, round: r.id, player, commitment, count, pool_tokens: r.pool_tokens });
    if r.close_at == 0 { schedule_close(r, now); }
    Ok(())
}

/// One more reveal of `n`: bumps its tally (["num", round, n]), creating it if it's the first.
fn bump_tally<'info>(acc: &AccountInfo<'info>, round: Pubkey, n: u64, payer: &AccountInfo<'info>, system: &Program<'info, System>) -> Result<()> {
    let nb = n.to_le_bytes();
    let (addr, bump) = Pubkey::find_program_address(&[b"num", round.as_ref(), nb.as_ref()], &crate::ID);
    require_keys_eq!(acc.key(), addr, GameError::WrongTally);
    require!(acc.is_writable, GameError::WrongTally);
    let mut t = if acc.owner == &crate::ID && !acc.data_is_empty() {
        Tally::try_deserialize(&mut &acc.try_borrow_data()?[..])?
    } else {
        // Transfer + allocate + assign, as for the player profile.
        let space = 8 + Tally::INIT_SPACE;
        let rent = Rent::get()?.minimum_balance(space);
        let have = acc.lamports();
        if have < rent {
            system_program::transfer(CpiContext::new(system.key(), system_program::Transfer { from: payer.clone(), to: acc.clone() }), rent - have)?;
        }
        let b = [bump];
        let seeds: &[&[&[u8]]] = &[&[b"num", round.as_ref(), nb.as_ref(), &b]];
        system_program::allocate(CpiContext::new_with_signer(system.key(), system_program::Allocate { account_to_allocate: acc.clone() }, seeds), space as u64)?;
        system_program::assign(CpiContext::new_with_signer(system.key(), system_program::Assign { account_to_assign: acc.clone() }, seeds), &crate::ID)?;
        Tally { round, number: n, count: 0, payer: payer.key(), bump }
    };
    t.count = t.count.saturating_add(1);
    t.try_serialize(&mut &mut acc.try_borrow_mut_data()?[..])?;
    Ok(())
}

/// Record `n` (revealed exactly once) as a claim. The round keeps the MAX_WINNERS lowest.
fn claim_number(r: &mut Round, n: u64, player: Pubkey) -> Result<()> {
    require!(!r.claims.iter().any(|c| c.number == n), GameError::AlreadyClaimed);
    require!(n < r.claims[MAX_WINNERS - 1].number, GameError::NotLowest);
    // Replace the highest slot, then bubble it into place (the list stays sorted, lowest first).
    let mut i = MAX_WINNERS - 1;
    r.claims[i] = Winner { number: n, player };
    while i > 0 && r.claims[i].number < r.claims[i - 1].number {
        r.claims.swap(i, i - 1);
        i -= 1;
    }
    emit!(Claimed { table: r.table_id, round: r.id, player, number: n, place: i as u8 + 1 });
    Ok(())
}

/// Close the round at the next scheduled time after `now` (never `now` itself, so there's
/// always a countdown).
fn schedule_close(r: &mut Round, now: i64) {
    r.close_at = next_close(now, r.period_secs, r.close_offset_secs);
    emit!(CloseScheduled { table: r.table_id, round: r.id, close_at: r.close_at });
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Rules shared by every table.
#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct ConfigParams {
    pub pool_bps: u16,
    /// Share of each ticket that goes to stakers.
    pub reserve_bps: u16,
    pub burn_bps: u16,
    /// Share of each referred ticket paid to the referrer, out of the team's share.
    pub referral_bps: u16,
    /// Entries needed for 2, 3 and 4 winners (u64::MAX switches a tier off).
    pub tier_min_entries: [u64; 3],
    /// Pool split by place for 1, 2, 3 and 4 winners. Row k pays k+1 places and sums to 10000.
    pub tier_splits: [[u16; 4]; 4],
    /// Receives forfeited reveal deposits (SOL).
    pub team_wallet: Pubkey,
    /// May call `set_ticket`, and nothing else.
    pub keeper: Pubkey,
    /// Largest ticket price change `set_ticket` may make at once (2500 = ±25%).
    pub max_price_change_bps: u16,
    /// Smallest stake anyone can hold (keeps the reward math well inside its range).
    pub min_stake_tokens: u64,
    /// Rewards that arrived while nobody was staked are released over this much staked time.
    pub backlog_release_secs: i64,
}

impl ConfigParams {
    fn validate(&self) -> Result<()> {
        require!(self.min_stake_tokens > 0, GameError::BadConfig);
        let taken = self.pool_bps as u64 + self.reserve_bps as u64 + self.burn_bps as u64 + self.referral_bps as u64;
        require!(taken <= BPS, GameError::BadConfig);
        // Guardrails, so the admin key can never quietly turn the game against the players:
        // the pool always gets at least 70% of every ticket, marketing (with referrals) at most
        // 10%, and the keeper can move a ticket price at most ±50% per round.
        require!(self.pool_bps as u64 >= MIN_POOL_BPS, GameError::BadConfig);
        require!(BPS - taken + self.referral_bps as u64 <= MAX_MARKETING_BPS, GameError::BadConfig);
        require!(self.max_price_change_bps as u64 <= MAX_PRICE_CHANGE_BPS && self.backlog_release_secs > 0, GameError::BadConfig);
        require!(self.tier_min_entries.windows(2).all(|w| w[0] <= w[1]), GameError::BadConfig);
        for (k, row) in self.tier_splits.iter().enumerate() {
            let sum: u64 = row.iter().map(|&x| x as u64).sum();
            require!(sum == BPS, GameError::BadConfig);
            require!(row[..=k].iter().all(|&x| x > 0) && row[k + 1..].iter().all(|&x| x == 0), GameError::BadConfig);
        }
        Ok(())
    }
}

/// One table's rules.
#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct TableParams {
    /// Tokens per pick, including decimals.
    pub ticket_tokens: u64,
    /// Rounds close at `close_offset_secs + k * period_secs` (3600 and 0 = hourly at :00).
    pub period_secs: i64,
    pub close_offset_secs: i64,
    pub reveal_secs: i64,
    pub claim_secs: i64,
    pub reveal_deposit_lamports: u64,
}

impl TableParams {
    fn validate(&self) -> Result<()> {
        // Windows long enough that players (and the keeper) can always get their reveals and
        // claims in: a too-short window would quietly turn every round into a rollover.
        require!(self.ticket_tokens > 0 && self.reveal_secs >= MIN_REVEAL_SECS && self.claim_secs >= MIN_CLAIM_SECS, GameError::BadConfig);
        require!(self.period_secs > 0 && self.close_offset_secs >= 0 && self.close_offset_secs < self.period_secs, GameError::BadConfig);
        Ok(())
    }
}

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    pub keeper: Pubkey,
    pub mint: Pubkey,
    pub team_token: Pubkey,
    pub team_wallet: Pubkey,
    pub pool_bps: u16,
    pub reserve_bps: u16,
    pub burn_bps: u16,
    pub referral_bps: u16,
    pub tier_min_entries: [u64; 3],
    pub tier_splits: [[u16; 4]; 4],
    pub max_price_change_bps: u16,
    pub min_stake_tokens: u64,
    pub backlog_release_secs: i64,
    pub entries_paused: bool,
    pub bump: u8,
    pub vault_bump: u8,
    pub reserve_bump: u8,
    pub stake_vault_bump: u8,
}

impl Config {
    fn apply(&mut self, p: &ConfigParams) {
        self.pool_bps = p.pool_bps;
        self.reserve_bps = p.reserve_bps;
        self.burn_bps = p.burn_bps;
        self.referral_bps = p.referral_bps;
        self.tier_min_entries = p.tier_min_entries;
        self.tier_splits = p.tier_splits;
        self.team_wallet = p.team_wallet;
        self.keeper = p.keeper;
        self.max_price_change_bps = p.max_price_change_bps;
        self.min_stake_tokens = p.min_stake_tokens;
        self.backlog_release_secs = p.backlog_release_secs;
    }
}

#[account]
#[derive(InitSpace)]
pub struct Table {
    pub id: u8,
    pub ticket_tokens: u64,
    /// v1 leftovers (capped seats, number range, minimum picks): unused, kept in place so
    /// accounts created before v2 still load. Always 0 from v2 on.
    pub legacy_max_seats: u32,
    pub legacy_max_number: u64,
    pub period_secs: i64,
    pub close_offset_secs: i64,
    pub legacy_min_picks: u32,
    pub reveal_secs: i64,
    pub claim_secs: i64,
    pub reveal_deposit_lamports: u64,
    pub enabled: bool,
    pub current_round: u64,
    /// When `set_ticket` was last used (at most every `TICKET_UPDATE_SECS`).
    pub ticket_set_at: i64,
    pub bump: u8,
}

impl Table {
    fn apply(&mut self, p: &TableParams) {
        self.ticket_tokens = p.ticket_tokens;
        self.legacy_max_seats = 0;
        self.legacy_max_number = 0;
        self.period_secs = p.period_secs;
        self.close_offset_secs = p.close_offset_secs;
        self.legacy_min_picks = 0;
        self.reveal_secs = p.reveal_secs;
        self.claim_secs = p.claim_secs;
        self.reveal_deposit_lamports = p.reveal_deposit_lamports;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase { Open, Reveal, Claim, Settleable, Settled }

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, InitSpace, PartialEq, Eq, Debug)]
pub struct Winner {
    pub number: u64,
    pub player: Pubkey,
}

const EMPTY: Winner = Winner { number: NO_CLAIM, player: Pubkey::new_from_array([0; 32]) };

/// Everything that decides how a round plays is copied in when it opens,
/// so rule changes never alter a round that's already running.
#[account]
#[derive(InitSpace)]
pub struct Round {
    pub table_id: u8,
    pub id: u64,
    pub ticket_tokens: u64,
    pub pool_tokens: u64,
    pub entries: u64,
    pub revealed: u64,
    pub opened_at: i64,
    /// When entries close (0 until the first pick).
    pub close_at: i64,
    /// v1 leftovers, unused (see Table).
    pub legacy_max_seats: u32,
    pub legacy_max_number: u64,
    pub period_secs: i64,
    pub close_offset_secs: i64,
    pub legacy_min_picks: u32,
    pub reveal_secs: i64,
    pub claim_secs: i64,
    pub deposit_lamports: u64,
    pub pool_bps: u16,
    pub reserve_bps: u16,
    pub burn_bps: u16,
    pub referral_bps: u16,
    pub tier_min_entries: [u64; 3],
    pub tier_splits: [[u16; 4]; 4],
    /// Lowest unique numbers claimed so far, lowest first. Empty slots have number = u64::MAX.
    pub claims: [Winner; 4],
    /// When the last entry was revealed (0 until then). Ends the reveal window early.
    pub all_revealed_at: i64,
    pub settled: bool,
    pub bump: u8,
}

impl Round {
    fn open(&mut self, t: &Table, cfg: &Config, id: u64, carry: u64, now: i64, bump: u8) {
        self.table_id = t.id;
        self.id = id;
        self.ticket_tokens = t.ticket_tokens;
        self.pool_tokens = carry;
        self.entries = 0;
        self.revealed = 0;
        self.opened_at = now;
        self.close_at = 0;
        self.legacy_max_seats = 0;
        self.legacy_max_number = 0;
        self.period_secs = t.period_secs;
        self.close_offset_secs = t.close_offset_secs;
        self.legacy_min_picks = 0;
        self.reveal_secs = t.reveal_secs;
        self.claim_secs = t.claim_secs;
        self.deposit_lamports = t.reveal_deposit_lamports;
        self.pool_bps = cfg.pool_bps;
        self.reserve_bps = cfg.reserve_bps;
        self.burn_bps = cfg.burn_bps;
        self.referral_bps = cfg.referral_bps;
        self.tier_min_entries = cfg.tier_min_entries;
        self.tier_splits = cfg.tier_splits;
        self.claims = [EMPTY; MAX_WINNERS];
        self.all_revealed_at = 0;
        self.settled = false;
        self.bump = bump;
    }

    /// How many places this round pays, from how many numbers were picked.
    pub fn places(&self) -> usize {
        1 + self.tier_min_entries.iter().filter(|&&m| self.entries >= m).count()
    }

    /// The reveal window ends after `reveal_secs`, or as soon as every entry is revealed.
    pub fn reveal_ends_at(&self) -> i64 {
        let full = self.close_at + self.reveal_secs;
        if self.all_revealed_at > 0 { full.min(self.all_revealed_at) } else { full }
    }
    pub fn claim_ends_at(&self) -> i64 { self.reveal_ends_at() + self.claim_secs }

    pub fn phase(&self, now: i64) -> Phase {
        if self.settled { Phase::Settled }
        else if self.close_at == 0 || now < self.close_at { Phase::Open }
        else if now < self.reveal_ends_at() { Phase::Reveal }
        else if now < self.claim_ends_at() { Phase::Claim }
        else { Phase::Settleable }
    }
}

#[account]
#[derive(InitSpace)]
pub struct Entry {
    /// The round account this pick belongs to.
    pub round: Pubkey,
    pub player: Pubkey,
    pub commitment: [u8; 32],
    pub backup: [u8; BACKUP_LEN],
    pub number: u64,
    pub revealed: bool,
    pub deposit: u64,
    pub bump: u8,
}

/// v6: up to MAX_BUNDLE numbers sealed under one commitment (see the header). `numbers` holds
/// them encrypted for the player's backup until the reveal, and the numbers themselves after.
/// Same layout start as Entry (round, then player), so the same filters find both.
#[account]
pub struct Bundle {
    pub round: Pubkey,
    pub player: Pubkey,
    pub commitment: [u8; 32],
    pub nonce: [u8; 16],
    pub revealed: bool,
    pub deposit: u64,
    pub bump: u8,
    pub numbers: Vec<u64>,
}

impl Bundle {
    pub fn space(k: usize) -> usize { 8 + 32 + 32 + 32 + 16 + 1 + 8 + 1 + 4 + 8 * k }
}

/// One wallet's player profile: who referred them (set once, on their first referred pick).
#[account]
#[derive(InitSpace)]
pub struct PlayerProfile {
    pub referrer: Pubkey,
    pub bump: u8,
}

/// How many revealed entries picked this number in this round.
#[account]
#[derive(InitSpace)]
pub struct Tally {
    /// The round account.
    pub round: Pubkey,
    pub number: u64,
    pub count: u32,
    pub payer: Pubkey,
    pub bump: u8,
}

/// Staking totals and the reward-per-token index.
#[account]
#[derive(InitSpace)]
pub struct StakePool {
    pub total_staked: u64,
    /// Rewards per staked token so far, times SCALE.
    pub acc_per_token: u128,
    /// Rewards that arrived while nobody was staked, not yet released.
    pub backlog: u64,
    /// When the backlog finishes releasing (pushed back while nobody is staked).
    pub stream_end: i64,
    pub last_update: i64,
    pub bump: u8,
}

impl StakePool {
    /// Bring the index up to `now`: release the due part of the backlog to current stakers.
    fn update(&mut self, now: i64) -> Result<()> {
        if now <= self.last_update { return Ok(()); }
        let dt = now - self.last_update;
        if self.backlog > 0 {
            if self.total_staked == 0 {
                // Nobody to pay: the release clock only runs while someone is staked.
                self.stream_end += dt;
            } else {
                let remaining = (self.stream_end - self.last_update).max(1);
                let release = if dt >= remaining {
                    self.backlog
                } else {
                    (self.backlog as u128 * dt as u128 / remaining as u128) as u64
                };
                self.distribute(release)?;
                self.backlog -= release;
            }
        }
        self.last_update = now;
        Ok(())
    }

    /// New rewards: straight to stakers, or into the backlog if nobody is staked.
    fn add_rewards(&mut self, amount: u64, now: i64, release_secs: i64) -> Result<()> {
        if amount == 0 { return Ok(()); }
        if self.total_staked > 0 {
            self.distribute(amount)
        } else {
            self.backlog = self.backlog.checked_add(amount).ok_or(GameError::Overflow)?;
            self.stream_end = now + release_secs;
            Ok(())
        }
    }

    fn distribute(&mut self, amount: u64) -> Result<()> {
        if amount == 0 || self.total_staked == 0 { return Ok(()); }
        let add = amount as u128 * SCALE / self.total_staked as u128;
        self.acc_per_token = self.acc_per_token.checked_add(add).ok_or(GameError::Overflow)?;
        Ok(())
    }
}

/// One wallet's stake.
#[account]
#[derive(InitSpace)]
pub struct Staker {
    pub owner: Pubkey,
    pub amount: u64,
    /// amount × index at the last sync, divided by SCALE: what's already been counted.
    pub debt: u128,
    /// Rewards earned and not yet claimed or spent.
    pub pending: u64,
    pub bump: u8,
}

impl Staker {
    fn owed(&self, acc: u128) -> Result<u128> {
        Ok((self.amount as u128).checked_mul(acc).ok_or(GameError::Overflow)? / SCALE)
    }

    /// Move everything earned since the last sync into `pending`.
    fn sync(&mut self, acc: u128) -> Result<()> {
        let owed = self.owed(acc)?;
        let earned = u64::try_from(owed.checked_sub(self.debt).ok_or(GameError::Overflow)?).map_err(|_| GameError::Overflow)?;
        self.pending = self.pending.checked_add(earned).ok_or(GameError::Overflow)?;
        self.debt = owed;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + Config::INIT_SPACE, seeds = [b"config"], bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(init, payer = admin, seeds = [b"vault"], bump,
        token::mint = mint, token::authority = config, token::token_program = token_program)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init, payer = admin, seeds = [b"reserve"], bump,
        token::mint = mint, token::authority = config, token::token_program = token_program)]
    pub reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init, payer = admin, seeds = [b"stake_vault"], bump,
        token::mint = mint, token::authority = config, token::token_program = token_program)]
    pub stake_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init, payer = admin, space = 8 + StakePool::INIT_SPACE, seeds = [b"stake_pool"], bump)]
    pub stake_pool: Box<Account<'info, StakePool>>,
    #[account(token::mint = mint, token::token_program = token_program)]
    pub team_token: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump = config.bump, has_one = admin, has_one = mint)]
    pub config: Box<Account<'info, Config>>,
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(token::mint = mint)]
    pub team_token: Box<InterfaceAccount<'info, TokenAccount>>,
}

#[derive(Accounts)]
#[instruction(id: u8)]
pub struct CreateTable<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = admin)]
    pub config: Box<Account<'info, Config>>,
    #[account(init, payer = admin, space = 8 + Table::INIT_SPACE, seeds = [b"table", id.to_le_bytes().as_ref()], bump)]
    pub table: Box<Account<'info, Table>>,
    #[account(init, payer = admin, space = 8 + Round::INIT_SPACE, seeds = [b"round", id.to_le_bytes().as_ref(), 1u64.to_le_bytes().as_ref()], bump)]
    pub round: Box<Account<'info, Round>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateTable<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = admin)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [b"table", table.id.to_le_bytes().as_ref()], bump = table.bump)]
    pub table: Box<Account<'info, Table>>,
}

#[derive(Accounts)]
pub struct SetTableEnabled<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = admin)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [b"table", table.id.to_le_bytes().as_ref()], bump = table.bump)]
    pub table: Box<Account<'info, Table>>,
    #[account(seeds = [b"round", table.id.to_le_bytes().as_ref(), table.current_round.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
}

#[derive(Accounts)]
pub struct SetTicket<'info> {
    /// The keeper or the admin (checked in the handler).
    pub signer: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [b"table", table.id.to_le_bytes().as_ref()], bump = table.bump)]
    pub table: Box<Account<'info, Table>>,
    /// The table's current round: its ticket follows while it's still open.
    #[account(mut, seeds = [b"round", table.id.to_le_bytes().as_ref(), table.current_round.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
}

#[derive(Accounts)]
pub struct StartClose<'info> {
    #[account(seeds = [b"table", table.id.to_le_bytes().as_ref()], bump = table.bump)]
    pub table: Box<Account<'info, Table>>,
    #[account(mut, seeds = [b"round", table.id.to_le_bytes().as_ref(), table.current_round.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump = config.bump, has_one = admin)]
    pub config: Box<Account<'info, Config>>,
}

#[derive(Accounts)]
#[instruction(commitment: [u8; 32])]
pub struct Enter<'info> {
    #[account(mut)]
    pub player: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = mint, has_one = team_token)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [b"table", table.id.to_le_bytes().as_ref()], bump = table.bump)]
    pub table: Box<Account<'info, Table>>,
    #[account(mut, seeds = [b"round", table.id.to_le_bytes().as_ref(), table.current_round.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    #[account(init, payer = player, space = 8 + Entry::INIT_SPACE,
        seeds = [b"entry", round.key().as_ref(), commitment.as_ref()], bump)]
    pub entry: Box<Account<'info, Entry>>,
    #[account(mut, mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = mint, token::authority = player, token::token_program = token_program)]
    pub player_token: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"vault"], bump = config.vault_bump)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"reserve"], bump = config.reserve_bump)]
    pub reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"stake_pool"], bump = stake_pool.bump)]
    pub stake_pool: Box<Account<'info, StakePool>>,
    #[account(mut)]
    pub team_token: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Optional: the referrer's token account, paid `referral_bps` of the ticket. Once a wallet
    /// has a referrer, only that referrer's account is accepted.
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub referrer_token: Option<Box<InterfaceAccount<'info, TokenAccount>>>,
    /// CHECK: the player's profile, ["player", wallet]. Holds their referrer; created by
    /// `bind_referrer` on their first referred pick, checked there before it's read.
    #[account(mut, seeds = [b"player", player.key().as_ref()], bump)]
    pub profile: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(commitment: [u8; 32])]
pub struct EnterFromStake<'info> {
    #[account(mut)]
    pub player: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = mint, has_one = team_token)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [b"table", table.id.to_le_bytes().as_ref()], bump = table.bump)]
    pub table: Box<Account<'info, Table>>,
    #[account(mut, seeds = [b"round", table.id.to_le_bytes().as_ref(), table.current_round.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    #[account(init, payer = player, space = 8 + Entry::INIT_SPACE,
        seeds = [b"entry", round.key().as_ref(), commitment.as_ref()], bump)]
    pub entry: Box<Account<'info, Entry>>,
    #[account(mut, seeds = [b"stake", player.key().as_ref()], bump = staker.bump)]
    pub staker: Box<Account<'info, Staker>>,
    #[account(mut, seeds = [b"stake_pool"], bump = stake_pool.bump)]
    pub stake_pool: Box<Account<'info, StakePool>>,
    #[account(mut, mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    /// Receives any leftover stake too small to keep staked.
    #[account(mut, token::mint = mint, token::authority = player, token::token_program = token_program)]
    pub player_token: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"stake_vault"], bump = config.stake_vault_bump)]
    pub stake_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"vault"], bump = config.vault_bump)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"reserve"], bump = config.reserve_bump)]
    pub reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub team_token: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Optional: the referrer's token account, paid `referral_bps` of the ticket. Once a wallet
    /// has a referrer, only that referrer's account is accepted.
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub referrer_token: Option<Box<InterfaceAccount<'info, TokenAccount>>>,
    /// CHECK: the player's profile, ["player", wallet]. Holds their referrer; created by
    /// `bind_referrer` on their first referred pick, checked there before it's read.
    #[account(mut, seeds = [b"player", player.key().as_ref()], bump)]
    pub profile: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(new_commitment: [u8; 32])]
pub struct Repick<'info> {
    #[account(mut)]
    pub player: Signer<'info>,
    #[account(seeds = [b"round", round.table_id.to_le_bytes().as_ref(), round.id.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    /// Closed at the end: rent and deposit go back to the player.
    #[account(mut, close = player, has_one = player,
        seeds = [b"entry", round.key().as_ref(), old_entry.commitment.as_ref()], bump = old_entry.bump)]
    pub old_entry: Box<Account<'info, Entry>>,
    #[account(init, payer = player, space = 8 + Entry::INIT_SPACE,
        seeds = [b"entry", round.key().as_ref(), new_commitment.as_ref()], bump)]
    pub new_entry: Box<Account<'info, Entry>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(number: u64)]
pub struct Reveal<'info> {
    /// Pays for a new number tally if this is the first reveal of that number.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [b"round", round.table_id.to_le_bytes().as_ref(), round.id.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    #[account(mut, seeds = [b"entry", round.key().as_ref(), entry.commitment.as_ref()], bump = entry.bump)]
    pub entry: Box<Account<'info, Entry>>,
    /// CHECK: receives the reveal deposit back; must be the entry's player.
    #[account(mut, address = entry.player)]
    pub player: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = payer, space = 8 + Tally::INIT_SPACE,
        seeds = [b"num", round.key().as_ref(), number.to_le_bytes().as_ref()], bump)]
    pub tally: Box<Account<'info, Tally>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    #[account(mut, seeds = [b"round", round.table_id.to_le_bytes().as_ref(), round.id.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    #[account(seeds = [b"entry", round.key().as_ref(), entry.commitment.as_ref()], bump = entry.bump)]
    pub entry: Box<Account<'info, Entry>>,
    #[account(seeds = [b"num", round.key().as_ref(), entry.number.to_le_bytes().as_ref()], bump = tally.bump)]
    pub tally: Box<Account<'info, Tally>>,
}

#[derive(Accounts)]
pub struct Settle<'info> {
    /// Pays rent for the next round's account.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = mint)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [b"table", table.id.to_le_bytes().as_ref()], bump = table.bump)]
    pub table: Box<Account<'info, Table>>,
    #[account(mut, seeds = [b"round", table.id.to_le_bytes().as_ref(), table.current_round.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    #[account(init, payer = payer, space = 8 + Round::INIT_SPACE,
        seeds = [b"round", table.id.to_le_bytes().as_ref(), (table.current_round + 1).to_le_bytes().as_ref()], bump)]
    pub next_round: Box<Account<'info, Round>>,
    #[account(mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds = [b"vault"], bump = config.vault_bump)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
    // remaining_accounts: winners' token accounts, lowest number first.
}

#[derive(Accounts)]
pub struct FundPool<'info> {
    pub funder: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = mint)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [b"table", table.id.to_le_bytes().as_ref()], bump = table.bump)]
    pub table: Box<Account<'info, Table>>,
    #[account(mut, seeds = [b"round", table.id.to_le_bytes().as_ref(), table.current_round.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    #[account(mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = mint, token::authority = funder, token::token_program = token_program)]
    pub funder_token: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"vault"], bump = config.vault_bump)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = mint)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [b"stake_pool"], bump = stake_pool.bump)]
    pub stake_pool: Box<Account<'info, StakePool>>,
    #[account(init_if_needed, payer = owner, space = 8 + Staker::INIT_SPACE, seeds = [b"stake", owner.key().as_ref()], bump)]
    pub staker: Box<Account<'info, Staker>>,
    #[account(mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = mint, token::authority = owner, token::token_program = token_program)]
    pub owner_token: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"stake_vault"], bump = config.stake_vault_bump)]
    pub stake_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Unstake<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = mint)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [b"stake_pool"], bump = stake_pool.bump)]
    pub stake_pool: Box<Account<'info, StakePool>>,
    #[account(mut, seeds = [b"stake", owner.key().as_ref()], bump = staker.bump)]
    pub staker: Box<Account<'info, Staker>>,
    #[account(mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = mint, token::authority = owner, token::token_program = token_program)]
    pub owner_token: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"stake_vault"], bump = config.stake_vault_bump)]
    pub stake_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct ClaimRewards<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = mint)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [b"stake_pool"], bump = stake_pool.bump)]
    pub stake_pool: Box<Account<'info, StakePool>>,
    #[account(mut, seeds = [b"stake", owner.key().as_ref()], bump = staker.bump)]
    pub staker: Box<Account<'info, Staker>>,
    #[account(mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = mint, token::authority = owner, token::token_program = token_program)]
    pub owner_token: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"reserve"], bump = config.reserve_bump)]
    pub reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct CloseEntry<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [b"round", round.table_id.to_le_bytes().as_ref(), round.id.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    #[account(mut, seeds = [b"entry", round.key().as_ref(), entry.commitment.as_ref()], bump = entry.bump)]
    pub entry: Box<Account<'info, Entry>>,
    /// CHECK: gets the rent back if the entry was revealed.
    #[account(mut, address = entry.player)]
    pub player: UncheckedAccount<'info>,
    /// CHECK: gets deposit + rent if the entry was never revealed.
    #[account(mut, address = config.team_wallet)]
    pub team_wallet: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct CloseTally<'info> {
    #[account(seeds = [b"round", round.table_id.to_le_bytes().as_ref(), round.id.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    #[account(mut, close = payer, has_one = payer,
        seeds = [b"num", round.key().as_ref(), tally.number.to_le_bytes().as_ref()], bump = tally.bump)]
    pub tally: Box<Account<'info, Tally>>,
    /// CHECK: whoever paid for the tally; checked by has_one.
    #[account(mut)]
    pub payer: UncheckedAccount<'info>,
}

#[derive(Accounts)]
#[instruction(commitment: [u8; 32], nonce: [u8; 16], sealed: Vec<u64>)]
pub struct EnterBundle<'info> {
    #[account(mut)]
    pub player: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = mint, has_one = team_token)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [b"table", table.id.to_le_bytes().as_ref()], bump = table.bump)]
    pub table: Box<Account<'info, Table>>,
    #[account(mut, seeds = [b"round", table.id.to_le_bytes().as_ref(), table.current_round.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    #[account(init, payer = player, space = Bundle::space(sealed.len()),
        seeds = [b"bundle", round.key().as_ref(), commitment.as_ref()], bump)]
    pub bundle: Box<Account<'info, Bundle>>,
    #[account(mut, mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = mint, token::authority = player, token::token_program = token_program)]
    pub player_token: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"vault"], bump = config.vault_bump)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"reserve"], bump = config.reserve_bump)]
    pub reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"stake_pool"], bump = stake_pool.bump)]
    pub stake_pool: Box<Account<'info, StakePool>>,
    #[account(mut)]
    pub team_token: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Optional: as in `Enter`.
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub referrer_token: Option<Box<InterfaceAccount<'info, TokenAccount>>>,
    /// CHECK: the player's profile, as in `Enter`.
    #[account(mut, seeds = [b"player", player.key().as_ref()], bump)]
    pub profile: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(commitment: [u8; 32], nonce: [u8; 16], sealed: Vec<u64>)]
pub struct EnterBundleFromStake<'info> {
    #[account(mut)]
    pub player: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = mint, has_one = team_token)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [b"table", table.id.to_le_bytes().as_ref()], bump = table.bump)]
    pub table: Box<Account<'info, Table>>,
    #[account(mut, seeds = [b"round", table.id.to_le_bytes().as_ref(), table.current_round.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    #[account(init, payer = player, space = Bundle::space(sealed.len()),
        seeds = [b"bundle", round.key().as_ref(), commitment.as_ref()], bump)]
    pub bundle: Box<Account<'info, Bundle>>,
    #[account(mut, seeds = [b"stake", player.key().as_ref()], bump = staker.bump)]
    pub staker: Box<Account<'info, Staker>>,
    #[account(mut, seeds = [b"stake_pool"], bump = stake_pool.bump)]
    pub stake_pool: Box<Account<'info, StakePool>>,
    #[account(mut, mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    /// Receives any leftover stake too small to keep staked.
    #[account(mut, token::mint = mint, token::authority = player, token::token_program = token_program)]
    pub player_token: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"stake_vault"], bump = config.stake_vault_bump)]
    pub stake_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"vault"], bump = config.vault_bump)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [b"reserve"], bump = config.reserve_bump)]
    pub reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub team_token: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Optional: as in `Enter`.
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub referrer_token: Option<Box<InterfaceAccount<'info, TokenAccount>>>,
    /// CHECK: the player's profile, as in `Enter`.
    #[account(mut, seeds = [b"player", player.key().as_ref()], bump)]
    pub profile: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(new_commitment: [u8; 32], nonce: [u8; 16], sealed: Vec<u64>)]
pub struct RepickBundle<'info> {
    #[account(mut)]
    pub player: Signer<'info>,
    #[account(seeds = [b"round", round.table_id.to_le_bytes().as_ref(), round.id.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    /// Closed at the end: rent and deposit go back to the player.
    #[account(mut, close = player, has_one = player,
        seeds = [b"bundle", round.key().as_ref(), old_bundle.commitment.as_ref()], bump = old_bundle.bump)]
    pub old_bundle: Box<Account<'info, Bundle>>,
    #[account(init, payer = player, space = Bundle::space(sealed.len()),
        seeds = [b"bundle", round.key().as_ref(), new_commitment.as_ref()], bump)]
    pub new_bundle: Box<Account<'info, Bundle>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RevealBundle<'info> {
    /// Pays for new number tallies.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [b"round", round.table_id.to_le_bytes().as_ref(), round.id.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    #[account(mut, seeds = [b"bundle", round.key().as_ref(), bundle.commitment.as_ref()], bump = bundle.bump)]
    pub bundle: Box<Account<'info, Bundle>>,
    /// CHECK: receives the reveal deposit back; must be the bundle's player.
    #[account(mut, address = bundle.player)]
    pub player: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    // remaining_accounts: each number's tally, in the bundle's order.
}

#[derive(Accounts)]
pub struct ClaimBundle<'info> {
    #[account(mut, seeds = [b"round", round.table_id.to_le_bytes().as_ref(), round.id.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    #[account(seeds = [b"bundle", round.key().as_ref(), bundle.commitment.as_ref()], bump = bundle.bump)]
    pub bundle: Box<Account<'info, Bundle>>,
    /// The claimed number's tally; its round and number are checked in the instruction.
    pub tally: Box<Account<'info, Tally>>,
}

#[derive(Accounts)]
pub struct CloseBundle<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [b"round", round.table_id.to_le_bytes().as_ref(), round.id.to_le_bytes().as_ref()], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    #[account(mut, seeds = [b"bundle", round.key().as_ref(), bundle.commitment.as_ref()], bump = bundle.bump)]
    pub bundle: Box<Account<'info, Bundle>>,
    /// CHECK: gets the rent back if the bundle was revealed.
    #[account(mut, address = bundle.player)]
    pub player: UncheckedAccount<'info>,
    /// CHECK: gets deposit + rent if the bundle was never revealed.
    #[account(mut, address = config.team_wallet)]
    pub team_wallet: UncheckedAccount<'info>,
}

// ---------------------------------------------------------------------------
// Events & errors
// ---------------------------------------------------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct WinnerPaid { pub player: Pubkey, pub number: u64, pub amount: u64 }

#[event]
pub struct TableCreated { pub table: u8 }
#[event]
pub struct TableEnabled { pub table: u8, pub enabled: bool }
#[event]
pub struct Entered { pub table: u8, pub round: u64, pub player: Pubkey, pub commitment: [u8; 32], pub pool_tokens: u64 }
#[event]
pub struct BundleEntered { pub table: u8, pub round: u64, pub player: Pubkey, pub commitment: [u8; 32], pub count: u8, pub pool_tokens: u64 }
#[event]
pub struct CloseScheduled { pub table: u8, pub round: u64, pub close_at: i64 }
#[event]
pub struct Repicked { pub table: u8, pub round: u64, pub player: Pubkey, pub old_commitment: [u8; 32], pub new_commitment: [u8; 32] }
#[event]
pub struct Revealed { pub table: u8, pub round: u64, pub player: Pubkey, pub number: u64 }
#[event]
pub struct Claimed { pub table: u8, pub round: u64, pub player: Pubkey, pub number: u64, pub place: u8 }
#[event]
pub struct Settled { pub table: u8, pub round: u64, pub pool_tokens: u64, pub winners: Vec<WinnerPaid>, pub rolled_over: u64 }
#[event]
pub struct Referred { pub table: u8, pub round: u64, pub player: Pubkey, pub referrer: Pubkey, pub amount: u64 }
#[event]
pub struct ReferrerBound { pub player: Pubkey, pub referrer: Pubkey }
#[event]
pub struct TicketSet { pub table: u8, pub round: u64, pub ticket_tokens: u64 }
#[event]
pub struct EntriesPaused { pub paused: bool }
#[event]
pub struct Staked { pub owner: Pubkey, pub amount: u64, pub total: u64 }
#[event]
pub struct Unstaked { pub owner: Pubkey, pub amount: u64, pub total: u64 }
#[event]
pub struct RewardsClaimed { pub owner: Pubkey, pub amount: u64 }
#[event]
pub struct PaidFromStake { pub player: Pubkey, pub from_rewards: u64, pub from_stake: u64, pub returned: u64 }

#[error_code]
pub enum GameError {
    #[msg("Entries are closed for this round")]
    EntriesClosed,
    #[msg("New picks are paused")]
    EntriesPaused,
    #[msg("This table is closed")]
    TableDisabled,
    #[msg("A table can only be switched off while its round is empty")]
    TableNotEmpty,
    #[msg("It's not reveal time")]
    NotRevealTime,
    #[msg("It's not claim time")]
    NotClaimTime,
    #[msg("Too early for that")]
    TooEarly,
    #[msg("Numbers start at 1")]
    BadNumber,
    #[msg("Already revealed")]
    AlreadyRevealed,
    #[msg("Number and salt don't match the sealed entry")]
    CommitmentMismatch,
    #[msg("Entry was never revealed")]
    NotRevealed,
    #[msg("Someone else revealed the same number")]
    NotUnique,
    #[msg("This number is already claimed")]
    AlreadyClaimed,
    #[msg("Enough lower unique numbers are already claimed")]
    NotLowest,
    #[msg("A winner's token account is missing")]
    WinnerAccountMissing,
    #[msg("That token account doesn't belong to the winner")]
    WrongWinnerAccount,
    #[msg("Only the keeper or admin can do that")]
    Unauthorized,
    #[msg("The ticket price was changed less than 10 minutes ago")]
    PriceAlreadySet,
    #[msg("That price change is bigger than allowed")]
    PriceChangeTooBig,
    #[msg("Amount must be more than zero")]
    BadAmount,
    #[msg("Not enough staked")]
    NotEnoughStaked,
    #[msg("A stake must be zero or at least the minimum")]
    BelowMinimumStake,
    #[msg("No rewards to claim")]
    NothingToClaim,
    #[msg("You can't refer yourself")]
    SelfReferral,
    #[msg("This wallet already has a different referrer")]
    WrongReferrer,
    #[msg("Picks can't be changed in the last minute before the round closes")]
    RepickClosed,
    #[msg("Invalid config")]
    BadConfig,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("The ticket price just changed: check the new price and try again")]
    TicketPriceChanged,
    #[msg("This round has no picks yet, or already has its close time")]
    NothingToStart,
    #[msg("A bundle holds 1 to 10 numbers")]
    BadBundle,
    #[msg("A changed bundle must keep the same number of picks")]
    BundleSizeChanged,
    #[msg("A number's tally account is missing or wrong")]
    WrongTally,
}
